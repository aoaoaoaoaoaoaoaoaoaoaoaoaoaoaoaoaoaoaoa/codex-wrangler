use std::{
    collections::BTreeSet,
    env, fs,
    io::Write as _,
    os::unix::fs::{PermissionsExt as _, symlink},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use brass_poolrooms::{
    chrome::{ForgePin, LonginusCursor},
    egui::CustomCursorImage,
};
use egui_tester::{
    AppCommand, Application, Button, Condition, Error as TesterError, Graphics, Key, Modifiers,
    Motion, ReactionBudget, Result, Story, Testbed, TestbedBuilder, Window, WindowQuery,
    X11CursorImage, X11Session, demand,
};
use rusqlite::{Connection, params};
use serde_json::Value;

mod showcase;
mod terminal_fixture;

use codex_wrangler_contract::{
    CardKey, CardObservation, CardTarget, ClosePreference, DeleteGuard, Flight, ForkField,
    GuideVisibility, Harness, HistoryColumn, HistoryOperation, HistorySortTarget, HistoryTarget,
    Observation, PinField, SearchTarget, SettingTarget, SortDirection, Tab, TabTarget,
    UI_FINGERPRINT, Work, WorkspaceTarget,
};

const GOAL: &str = "10000000-0000-7000-8000-000000000001";
const TURN: &str = "20000000-0000-7000-8000-000000000002";
const DONE: &str = "30000000-0000-7000-8000-000000000003";
const INPUT: &str = "40000000-0000-7000-8000-000000000004";
const CLAUDE: &str = "50000000-0000-7000-8000-000000000005";
const PRIME: &str = "60000000-0000-7000-8000-000000000006";
const PERMISSION: &str = "70000000-0000-7000-8000-000000000007";
const ROTATE: &str = "80000000-0000-7000-8000-000000000008";
const DORMANT: &str = "90000000-0000-7000-8000-000000000009";
const UNSEEN: &str = "a0000000-0000-7000-8000-00000000000a";
const ERROR: &str = "b0000000-0000-7000-8000-00000000000b";
const COLD: &str = "c0000000-0000-7000-8000-00000000000c";
const FRESH: &str = "d0000000-0000-7000-8000-00000000000d";
const RENAMED_HISTORY: &str = "Copper archive";
const RENAMED_ARCHIVED_HISTORY: &str = "Obsidian archive";
const OLD_RESET: i64 = 1_000_000;
const NEW_RESET: i64 = 2_000_000;
const FUNCTIONAL_ACCEPTANCE_ENV: &str = "CODEX_WRANGLER_FUNCTIONAL_ACCEPTANCE";
const INPUT_REACTION_CEILING: Duration = Duration::from_millis(75);
const FUNCTIONAL_INPUT_TIMEOUT: Duration = Duration::from_secs(2);
const FIXTURE_POLL_INTERVAL_MILLIS: u64 = 20;
const I3_READINESS_POLLS: u64 = 100;
const TERMINAL_READINESS_POLLS: u64 = 500;
const APPLICATION_STARTUP_ALLOWANCE_MILLIS: u64 = 8_000;
const APPLICATION_APPEARANCE_CEILING: Duration = Duration::from_millis(
    FIXTURE_POLL_INTERVAL_MILLIS * (I3_READINESS_POLLS + TERMINAL_READINESS_POLLS)
        + APPLICATION_STARTUP_ALLOWANCE_MILLIS,
);
const APPLICATION_RUNTIME: Duration = Duration::from_mins(3);
const RETAINED_FAILURE_ARTIFACTS: usize = 3;

fn main() -> Result<()> {
    if terminal_fixture::invoked()? {
        return terminal_fixture::serve();
    }
    let binary = sibling_binary()?;
    let failure_artifacts = failure_artifact_directory()?;
    let showcase = env::var_os("CODEX_WRANGLER_SHOWCASE_CAPTURE");
    TestbedBuilder::default()
        .failure_artifacts(failure_artifacts)
        .run(|testbed| match &showcase {
            Some(destination) => showcase::story(testbed, &binary, Path::new(destination)),
            None => story(testbed, &binary),
        })
}

fn failure_artifact_directory() -> Result<PathBuf> {
    if let Some(root) = env::var_os("FOUNDRY_EVIDENCE_DIR") {
        return Ok(PathBuf::from(root).join("acceptance-failure"));
    }
    let root = if let Some(root) = env::var_os("XDG_STATE_HOME") {
        PathBuf::from(root)
            .join("codex-wrangler")
            .join("acceptance-failure")
    } else {
        let home = env::var_os("HOME").ok_or_else(|| TesterError::Verdict {
            detail: "acceptance failure artifacts require HOME or XDG_STATE_HOME".to_owned(),
        })?;
        PathBuf::from(home).join(".local/state/codex-wrangler/acceptance-failure")
    };
    fs::create_dir_all(&root)
        .map_err(|error| artifact_io("create acceptance-failure directory", &error))?;
    prune_failure_artifacts(&root)?;
    Ok(root)
}

fn prune_failure_artifacts(root: &Path) -> Result<()> {
    let mut dead = fs::read_dir(root)
        .map_err(|error| artifact_io("read acceptance-failure directory", &error))?
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .and_then(|name| name.split_once('-'))
                .and_then(|(pid, _)| pid.parse::<u32>().ok())
                .is_none_or(|pid| !Path::new("/proc").join(pid.to_string()).exists())
        })
        .filter_map(|entry| {
            entry
                .metadata()
                .ok()?
                .modified()
                .ok()
                .map(|modified| (modified, entry.path()))
        })
        .collect::<Vec<_>>();
    dead.sort_unstable_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (_, path) in dead.into_iter().skip(RETAINED_FAILURE_ARTIFACTS) {
        fs::remove_dir_all(&path).map_err(|error| {
            artifact_io(
                format!("remove stale failure artifact `{}`", path.display()).as_str(),
                &error,
            )
        })?;
    }
    Ok(())
}

fn artifact_io(operation: &str, error: &std::io::Error) -> TesterError {
    TesterError::Verdict {
        detail: format!("{operation}: {error}"),
    }
}

fn input_reaction_budget() -> ReactionBudget {
    if env::var_os(FUNCTIONAL_ACCEPTANCE_ENV).is_some() {
        ReactionBudget::functional(FUNCTIONAL_INPUT_TIMEOUT)
    } else {
        ReactionBudget::performance(INPUT_REACTION_CEILING)
    }
}

fn story(testbed: &Testbed, binary: &Path) -> Result<()> {
    let fixture = Fixture::forge(testbed, binary)?;
    let app = testbed.launch(
        AppCommand::new(&fixture.wrapper)
            .borrow_read_only(binary)
            .graphics(Graphics::Host)
            .private_env("CODEX_HOME", "home/.codex")
            .witness("probes/wrangler")
            .runtime(APPLICATION_RUNTIME),
    )?;
    let wrangler = wait_named_window(
        testbed,
        &app,
        "Codex Wrangler",
        APPLICATION_APPEARANCE_CEILING,
    )?;
    verify_switcher_present(testbed, &fixture, wrangler.id(), false)?;
    let mut story: Story<'_, '_, Observation> = Story::bind(
        testbed,
        &app,
        WindowQuery::title_exact("Codex Wrangler"),
        ReactionBudget::functional(Duration::from_secs(10)),
    )?;
    verify_window_posture(testbed, &fixture, wrangler.id(), "tiled")?;
    verify_switcher_present(testbed, &fixture, wrangler.id(), false)?;
    let _floated = story.session().key(Key::Function(8))?;
    app.wait_until(
        Duration::from_secs(8),
        "Wrangler to become floating after its restored tiled opening",
        || Ok(fixture.floating_proof.is_file()),
    )?;
    verify_window_posture(testbed, &fixture, wrangler.id(), "floating")?;
    verify_gallery(testbed, &mut story, &fixture)?;
    verify_application_header(&mut story)?;

    let target = story.anchor(CardTarget(Harness::Codex, GOAL))?;
    let _clicked = story
        .session()
        .click(target.center().0, target.center().1, Button::Primary)?;
    app.wait_until(
        Duration::from_secs(8),
        "Wrangler to conceal itself after choosing a Codex terminal",
        || Ok(wrangler_count(testbed)? == Some(0)),
    )?;
    app.wait_until(
        Duration::from_secs(8),
        "floating posture to reach XDG state before concealment",
        || Ok(saved_posture(&fixture.state) == Some("floating")),
    )?;
    verify_hidden_cpu(&app)?;
    app.wait_until(
        Duration::from_secs(8),
        "tile to activate its Codex TTY",
        || {
            let _key = story.session().key(Key::Character('x'))?;
            Ok(fixture.proof.is_file())
        },
    )?;
    demand(
        fs::read_to_string(&fixture.proof).is_ok_and(|proof| proof.trim() == "x"),
        "the activated terminal did not receive native keyboard input",
    )?;
    verify_residency(testbed, binary, &app, &mut story, &fixture)
}

fn verify_application_header(story: &mut Story<'_, '_, Observation>) -> Result<()> {
    let header = story.anchor("eternalist.application.header")?.rect;
    let name = story.anchor("eternalist.application.name")?.rect;
    let help = story.anchor("eternalist.application.help")?.rect;
    let settings = story.anchor("eternalist.settings.open")?.rect;
    demand(
        header[0] <= name[0]
            && name[2] < help[0]
            && help[2] <= settings[0]
            && settings[2] >= header[2] - 0.5
            && name[1] <= help[3]
            && help[1] <= name[3],
        "application header did not present NAME, Help, Settings in canonical order",
    )
}

fn verify_hidden_cpu(app: &Application<'_>) -> Result<()> {
    let sample_seconds = env::var("CODEX_WRANGLER_CPU_SAMPLE_SECONDS")
        .ok()
        .and_then(|seconds| seconds.parse::<u64>().ok())
        .unwrap_or(5);
    thread::sleep(Duration::from_millis(250));
    let pids = application_processes(app.unit())?;
    let before = pids
        .iter()
        .map(|pid| cpu_ticks(*pid))
        .sum::<Result<u64>>()?;
    thread::sleep(Duration::from_secs(sample_seconds));
    let after = pids
        .iter()
        .map(|pid| cpu_ticks(*pid))
        .sum::<Result<u64>>()?;
    let consumed = after.saturating_sub(before);
    demand(
        consumed <= sample_seconds,
        format!(
            "hidden Wrangler pids {pids:?} consumed {consumed} CPU ticks in {sample_seconds} seconds ({before} -> {after})"
        ),
    )
}

fn application_processes(unit: &str) -> Result<Vec<u32>> {
    let status =
        fs::read_to_string("/proc/self/status").map_err(io_verdict("read acceptance uid"))?;
    let uid = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|uids| uids.split_ascii_whitespace().next())
        .ok_or_else(|| TesterError::Verdict {
            detail: "process status omitted its uid".to_owned(),
        })?;
    let runtime = format!("/run/user/{uid}");
    let output = Command::new("systemctl")
        .env("XDG_RUNTIME_DIR", &runtime)
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={runtime}/bus"),
        )
        .args(["--user", "show", unit, "-p", "ControlGroup", "--value"])
        .output()
        .map_err(io_verdict("query acceptance cgroup"))?;
    demand(
        output.status.success(),
        format!(
            "could not query acceptance cgroup: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    )?;
    let control_group = String::from_utf8_lossy(&output.stdout);
    let processes = fs::read_to_string(
        Path::new("/sys/fs/cgroup")
            .join(control_group.trim().trim_start_matches('/'))
            .join("cgroup.procs"),
    )
    .map_err(io_verdict("read acceptance cgroup"))?
    .split_ascii_whitespace()
    .filter_map(|pid| pid.parse::<u32>().ok())
    .filter(|pid| {
        fs::read_to_string(format!("/proc/{pid}/comm"))
            .is_ok_and(|comm| comm.trim() == "codex-wrangler")
    })
    .collect::<Vec<_>>();
    demand(
        !processes.is_empty(),
        "acceptance cgroup omitted the Wrangler process",
    )?;
    Ok(processes)
}

fn cpu_ticks(pid: u32) -> Result<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))
        .map_err(io_verdict("read Wrangler CPU ticks"))?;
    let fields = stat
        .rsplit_once(") ")
        .map(|(_, fields)| fields.split_ascii_whitespace().collect::<Vec<_>>())
        .ok_or_else(|| TesterError::Verdict {
            detail: format!("malformed process stat for Wrangler pid {pid}"),
        })?;
    let tick = |index: usize| {
        fields
            .get(index)
            .and_then(|field| field.parse::<u64>().ok())
            .ok_or_else(|| TesterError::Verdict {
                detail: format!("process stat omitted CPU field {index} for Wrangler pid {pid}"),
            })
    };
    Ok(tick(11)? + tick(12)?)
}

fn verify_residency(
    testbed: &Testbed,
    binary: &Path,
    app: &Application<'_>,
    story: &mut Story<'_, '_, Observation>,
    fixture: &Fixture,
) -> Result<()> {
    let x11 = testbed.x11()?;
    let wrangler_id = story.session().window().id();
    let tray = wait_named_window(testbed, app, "Codex Wrangler tray", Duration::from_secs(8))?;
    let tray = verify_tray_recovery(testbed, app, &tray)?;
    let _switched = story.session().key(Key::Function(10))?;
    app.wait_until(
        Duration::from_secs(8),
        "i3 to enter a different workspace",
        || Ok(fixture.workspace_proof.is_file()),
    )?;
    let _clicked = x11.click(&tray, 12, 12, Button::Primary)?;
    app.wait_until(
        Duration::from_secs(8),
        "tray click to reveal the gallery",
        || Ok(wrangler_count(testbed)?.is_some_and(|count| count > 0)),
    )?;
    verify_switcher_present(testbed, fixture, wrangler_id, false)?;

    let _closed = story.session().key(Key::Function(3))?;
    app.wait_until(
        Duration::from_secs(8),
        "window-manager close to conceal rather than kill Wrangler",
        || Ok(wrangler_count(testbed)? == Some(0)),
    )?;
    app.ensure_running("Wrangler to survive its window-manager close")?;
    let _switched = story.session().key(Key::Function(4))?;
    app.wait_until(
        Duration::from_secs(8),
        "i3 to enter the launcher's workspace",
        || Ok(fixture.launch_workspace_proof.is_file()),
    )?;
    let relay = testbed.launch(
        AppCommand::new(binary)
            .private_env("CODEX_HOME", "home/.codex")
            .runtime(Duration::from_secs(15)),
    )?;
    let relay_exit = relay.wait(Duration::from_secs(8))?;
    demand(
        relay_exit.success(),
        format!("second launch did not relay cleanly: {relay_exit:?}"),
    )?;
    app.wait_until(
        Duration::from_secs(8),
        "second launch to summon the incumbent Wrangler",
        || Ok(wrangler_count(testbed)?.is_some_and(|count| count > 0)),
    )?;
    app.wait_until(
        Duration::from_secs(8),
        "exactly one visible Wrangler after the relayed launch",
        || Ok(wrangler_count(testbed)? == Some(1)),
    )?;
    verify_switcher_present(testbed, fixture, wrangler_id, false)?;
    verify_tiled_mode(testbed, app, story, fixture, &tray, wrangler_id)?;

    let _menu_opened = x11.click(&tray, 12, 12, Button::Secondary)?;
    let menu = wait_named_window(
        testbed,
        app,
        "Codex Wrangler tray menu",
        Duration::from_secs(8),
    )?;
    let _quit = x11.click(&menu, 70, 15, Button::Primary)?;
    let exit = app.wait(Duration::from_secs(8))?;
    demand(
        exit.success(),
        format!("tray quit did not end Wrangler cleanly: {exit:?}"),
    )?;
    demand(
        saved_posture(&fixture.state) == Some("tiled"),
        "tray quit did not preserve the final tiled posture in XDG state",
    )
}

fn verify_tray_recovery(testbed: &Testbed, app: &Application<'_>, tray: &Window) -> Result<Window> {
    let redocked = testbed
        .x11()?
        .evict_and_wait_redocked(app, tray, Duration::from_secs(4))?;
    app.ensure_running("Wrangler to survive system-tray eviction")?;
    Ok(redocked)
}

fn verify_switcher_present(
    testbed: &Testbed,
    fixture: &Fixture,
    window: u32,
    fixed_desktop: bool,
) -> Result<()> {
    let mut command = AppCommand::new(&fixture.focus_probe).arg(window.to_string());
    if fixed_desktop {
        command = command.arg("/test/fixed-desktop");
    }
    let probe = testbed.launch(command.runtime(Duration::from_secs(6)))?;
    let exit = probe.wait(Duration::from_secs(6))?;
    demand(
        exit.success(),
        format!("Wrangler did not converge on the current workspace with focus: {exit:?}"),
    )
}

fn verify_window_posture(
    testbed: &Testbed,
    fixture: &Fixture,
    window: u32,
    expected: &str,
) -> Result<()> {
    let probe = testbed.launch(
        AppCommand::new(&fixture.posture_probe)
            .arg(window.to_string())
            .arg(expected)
            .runtime(Duration::from_secs(6)),
    )?;
    let exit = probe.wait(Duration::from_secs(6))?;
    demand(
        exit.success(),
        format!("Wrangler did not open {expected}: {exit:?}"),
    )
}

fn saved_posture(path: &Path) -> Option<&str> {
    match fs::read_to_string(path).ok()?.trim() {
        "floating" => Some("floating"),
        "tiled" => Some("tiled"),
        _ => None,
    }
}

fn verify_tiled_mode(
    testbed: &Testbed,
    app: &Application<'_>,
    story: &mut Story<'_, '_, Observation>,
    fixture: &Fixture,
    tray: &Window,
    wrangler: u32,
) -> Result<()> {
    let _tiled = story.session().key(Key::Function(5))?;
    app.wait_until(Duration::from_secs(8), "Wrangler to become tiled", || {
        Ok(fixture.tiled_proof.is_file())
    })?;
    let recorder = testbed.launch(
        AppCommand::new(&fixture.desktop_recorder)
            .arg(wrangler.to_string())
            .arg("/test/fixed-desktop")
            .runtime(Duration::from_secs(6)),
    )?;
    let recorder_exit = recorder.wait(Duration::from_secs(6))?;
    demand(
        recorder_exit.success(),
        format!("could not record tiled Wrangler desktop: {recorder_exit:?}"),
    )?;

    let target = story.anchor(CardTarget(Harness::Codex, TURN))?;
    let _selected = story
        .session()
        .click(target.center().0, target.center().1, Button::Primary)?;
    app.wait_until(
        Duration::from_secs(8),
        "card selection to leave the fixed Wrangler workspace",
        || Ok(wrangler_count(testbed)? == Some(0)),
    )?;
    let _returned = story.session().key(Key::Function(7))?;
    app.wait_until(
        Duration::from_secs(8),
        "i3 to return to the fixed Wrangler workspace",
        || Ok(fixture.tiled_home_proof.is_file()),
    )?;
    app.wait_until(
        Duration::from_secs(8),
        "tiled Wrangler to survive a card selection",
        || Ok(wrangler_count(testbed)? == Some(1)),
    )?;
    verify_switcher_present(testbed, fixture, wrangler, true)?;

    let _departed = story.session().key(Key::Function(6))?;
    app.wait_until(
        Duration::from_secs(8),
        "i3 to leave the fixed Wrangler workspace",
        || Ok(fixture.tiled_away_proof.is_file()),
    )?;
    let _summoned = testbed.x11()?.click(tray, 12, 12, Button::Primary)?;
    verify_switcher_present(testbed, fixture, wrangler, true)?;
    verify_session_lifecycle(testbed, app, story, fixture)
}

fn verify_session_lifecycle(
    testbed: &Testbed,
    app: &Application<'_>,
    story: &mut Story<'_, '_, Observation>,
    fixture: &Fixture,
) -> Result<()> {
    verify_remembrance(fixture)?;
    verify_history_open(testbed, story)?;
    verify_management_veto(story, fixture)?;
    pin_and_unpin(story, fixture)?;
    fork_and_return(testbed, story, app, fixture)?;
    select_and_return(
        testbed,
        story,
        app,
        ROTATE,
        &fixture.rotate_resume,
        &fixture.roster,
    )?;
    demand(
        fs::read_to_string(&fixture.rotate_resume).is_ok_and(|proof| proof.trim() == "resume 7"),
        "rolled session did not resume on its terminal workspace",
    )?;
    demand(
        read_roster(&fixture.roster)?["sessions"][ROTATE]["account"]["account"].is_string(),
        "rolled session was not rebound to the current Codex account",
    )?;

    select_and_return(
        testbed,
        story,
        app,
        DORMANT,
        &fixture.dormant_resume,
        &fixture.roster,
    )?;
    demand(
        fs::read_to_string(&fixture.dormant_resume).is_ok_and(|proof| proof.trim() == "resume 6"),
        "resurrected session did not return to its remembered workspace",
    )?;
    demand(
        thread_archived(&fixture.index, DORMANT) == Some(true),
        "reopening a closed session mutated Codex archive state",
    )?;

    select_and_return(
        testbed,
        story,
        app,
        DONE,
        &fixture.version_resume,
        &fixture.roster,
    )?;
    demand(
        fs::read_to_string(&fixture.version_resume).is_ok_and(|proof| proof.trim() == "resume 7"),
        "superseded Codex session did not roll onto the installed version",
    )?;
    demand(
        read_roster(&fixture.roster)?["sessions"][DONE]["cli_version"] == "0.147.0",
        "rolled session did not bind its launched Codex version",
    )?;

    shift_click_card(story, DONE)?;
    let closed = wait_card(story, DONE, |card| card.work == Work::Closed)?;
    demand(
        closed.work == Work::Closed,
        "closed session retained a live-terminal work state",
    )?;
    demand(
        thread_archived(&fixture.index, DONE) == Some(false)
            && fixture.done_rollout.is_file()
            && read_roster(&fixture.roster)?["sessions"][DONE].is_object()
            && read_roster(&fixture.roster)?["sessions"][DONE]
                .get("retention")
                .is_none(),
        "closing a session mutated Codex archive storage or lost remembered membership",
    )?;

    shift_click_card(story, DONE)?;
    let _gone = story.wait_stable(
        Duration::from_secs(8),
        Duration::from_millis(150),
        "forgotten closed session to leave the gallery",
        |frame| {
            frame
                .state
                .cards
                .iter()
                .all(|card| card.thread != DONE)
                .then_some(())
        },
    )?;
    demand(
        read_roster(&fixture.roster)?["sessions"]
            .get(DONE)
            .is_none()
            && thread_archived(&fixture.index, DONE) == Some(false),
        "forgetting a closed session mutated Codex storage or was not sealed immediately",
    )?;
    Ok(())
}

fn verify_remembrance(fixture: &Fixture) -> Result<()> {
    let state = read_roster(&fixture.roster)?;
    let sessions = state["sessions"]
        .as_object()
        .ok_or_else(|| TesterError::Verdict {
            detail: "known-session state omitted its session map".to_owned(),
        })?;
    demand(
        sessions.contains_key(DORMANT) && !sessions.contains_key(UNSEEN),
        "Wrangler did not preserve its remembered session boundary",
    )?;
    demand(
        thread_archived(&fixture.index, DORMANT) == Some(true),
        "fixture did not oppose Codex archive state to Wrangler closure",
    )?;
    Ok(())
}

fn fork_and_return(
    testbed: &Testbed,
    story: &mut Story<'_, '_, Observation>,
    app: &Application<'_>,
    fixture: &Fixture,
) -> Result<()> {
    story.session().focus()?;
    let (strike_x, strike_y) = seize_card(story, TURN, "fork")?;
    let control_down = story.session().key_down(Key::Control)?;
    let _yearning = story
        .reaction(control_down)
        .within(input_reaction_budget())
        .until(Condition::new(
            "held Ctrl to arm the Longinus fork field",
            |state: &Observation| {
                state.fork_field == ForkField::Armed
                    && !state.jiggling
                    && hovered(state, Harness::Codex, TURN)
            },
        ))?;
    let forked = story.session().click(strike_x, strike_y, Button::Primary)?;
    let _armed = story.reaction(forked).until(Condition::new(
        "Ctrl+click to enter fork flight",
        |state: &Observation| state.flight == Flight::Striking,
    ))?;
    let _control_up = story.session().key_up(Key::Control)?;
    app.wait_until(
        Duration::from_secs(10),
        "Codex chat to fork in a fresh Alacritty",
        || Ok(fixture.fork_launch.is_file()),
    )?;
    app.wait_until(
        Duration::from_secs(8),
        "fork launch to leave the fixed Wrangler workspace",
        || Ok(wrangler_count(testbed)? == Some(0)),
    )?;
    let _returned = story.session().key(Key::Function(7))?;
    app.wait_until(
        Duration::from_secs(8),
        "i3 to return to the fixed Wrangler workspace after fork",
        || Ok(wrangler_count(testbed)? == Some(1)),
    )?;
    let _landed = story.wait(Condition::new(
        "Codex fork strike to leave flight",
        |state: &Observation| state.flight == Flight::Grounded,
    ))?;
    demand(
        fs::read_to_string(&fixture.fork_launch).is_ok_and(|proof| proof.trim() == "fork 7"),
        "forked session did not launch on its source terminal workspace",
    )?;
    let original = wait_card(story, TURN, |card| card.work == Work::Delegated)?;
    demand(
        original.work == Work::Delegated,
        "forking a session displaced its original live terminal",
    )?;
    Ok(())
}

fn pin_and_unpin(story: &mut Story<'_, '_, Observation>, fixture: &Fixture) -> Result<()> {
    let (x, y) = seize_card(story, DONE, "pin")?;
    let alt_down = story.session().key_down(Key::Alt)?;
    let _settled = story
        .reaction(alt_down)
        .within(input_reaction_budget())
        .until(Condition::new(
            "held Alt to arm the immobile Forge Pin field",
            |state: &Observation| {
                state.pin_field == PinField::Armed
                    && state.fork_field == ForkField::Quiescent
                    && !state.jiggling
                    && hovered(state, Harness::Codex, DONE)
            },
        ))?;
    let clicked = story.session().click(x, y, Button::Primary)?;
    let _flight = story.reaction(clicked).until(Condition::new(
        "Alt+click to submit the pin",
        |state: &Observation| state.flight == Flight::Striking,
    ))?;
    let _alt_up = story.session().key_up(Key::Alt)?;
    vacate_gallery(story, "pointer to release the pinned card")?;
    let _pinned = story.wait(Condition::new(
        "pinned session to enter the head bucket",
        |state: &Observation| {
            state
                .cards
                .iter()
                .any(|card| card.thread == DONE && card.pinned)
                && state
                    .visible
                    .first()
                    .is_some_and(|card| card.thread == DONE)
        },
    ))?;
    let persisted = fs::read_to_string(&fixture.pinboard).unwrap_or_default();
    demand(
        persisted.contains(DONE),
        "pin did not reach XDG state before acknowledgement",
    )?;

    let (x, y) = seize_card(story, DONE, "unpin")?;
    let alt_down = story.session().key_down(Key::Alt)?;
    let _settled = story
        .reaction(alt_down)
        .within(input_reaction_budget())
        .until(Condition::new(
            "held Alt to arm the immobile Forge Pin field on a pinned tile",
            |state: &Observation| {
                state.pin_field == PinField::Armed && hovered(state, Harness::Codex, DONE)
            },
        ))?;
    let clicked = story.session().click(x, y, Button::Primary)?;
    let _flight = story.reaction(clicked).until(Condition::new(
        "Alt+click to submit the unpin",
        |state: &Observation| state.flight == Flight::Striking,
    ))?;
    let _alt_up = story.session().key_up(Key::Alt)?;
    vacate_gallery(story, "pointer to release the unpinned card")?;
    let _unpinned = story.wait(Condition::new(
        "unpinned session to leave the head bucket",
        |state: &Observation| {
            state
                .cards
                .iter()
                .any(|card| card.thread == DONE && !card.pinned)
                && state
                    .visible
                    .first()
                    .is_some_and(|card| card.thread != DONE)
        },
    ))?;
    Ok(())
}

fn verify_management_veto(story: &mut Story<'_, '_, Observation>, fixture: &Fixture) -> Result<()> {
    let shift_down = story.session().key_down(Key::Shift)?;
    let _jiggling = story
        .reaction(shift_down)
        .within(input_reaction_budget())
        .until(Condition::new(
            "held Shift to animate management mode",
            |state: &Observation| state.jiggling,
        ))?;
    let shift_up = story.session().key_up(Key::Shift)?;
    let _settled = story
        .reaction(shift_up)
        .within(input_reaction_budget())
        .until(Condition::new(
            "released Shift to still the management mode",
            |state: &Observation| !state.jiggling,
        ))?;

    for thread in [ERROR, GOAL, TURN, INPUT, PERMISSION] {
        shift_click_ignored(story, thread)?;
        thread::sleep(Duration::from_millis(250));
        let frame = story.frame()?;
        demand(
            frame
                .state
                .cards
                .iter()
                .any(|card| card.thread == thread && card.work != Work::Closed),
            format!("Shift retired active Codex session {thread}"),
        )?;
        demand(
            thread_archived(&fixture.index, thread) == Some(false),
            format!("Shift mutated Codex archive state for active session {thread}"),
        )?;
    }
    Ok(())
}

fn shift_click_ignored(story: &mut Story<'_, '_, Observation>, thread: &str) -> Result<()> {
    story.session().focus()?;
    let target = story.anchor(CardTarget(Harness::Codex, thread))?;
    let (center_x, center_y) = target.center();
    let x = center_x.saturating_sub(100);
    let moved = story.session().move_to(x, center_y)?;
    let sought = thread.to_owned();
    let _hovered = story.reaction(moved).until(Condition::new(
        "running management card to acquire the pointer",
        move |state: &Observation| hovered(state, Harness::Codex, &sought),
    ))?;
    let shift_down = story.session().key_down(Key::Shift)?;
    let _jiggling = story.reaction(shift_down).until(Condition::new(
        "management mode to acquire Shift over running work",
        |state: &Observation| state.jiggling,
    ))?;
    let clicked = story.session().click(x, center_y, Button::Primary)?;
    let click_completed = clicked.completed_ns();
    let _vetoed = story.wait_stable(
        input_reaction_budget().functional_timeout(),
        Duration::ZERO,
        "held Shift management click to settle without flight",
        |frame| {
            (frame.begun_ns >= click_completed
                && frame.state.jiggling
                && frame.state.flight == Flight::Grounded)
                .then_some(())
        },
    )?;
    let shift_up = story.session().key_up(Key::Shift)?;
    let _stilled = story.reaction(shift_up).until(Condition::new(
        "management mode to release Shift over running work",
        |state: &Observation| !state.jiggling,
    ))?;
    vacate_gallery(story, "running management pointer to vacate its card")
}

fn shift_click_card(story: &mut Story<'_, '_, Observation>, thread: &str) -> Result<()> {
    story.session().focus()?;
    let (x, y) = seize_card(story, thread, "management")?;
    let shift_down = story.session().key_down(Key::Shift)?;
    let _jiggling = story.reaction(shift_down).until(Condition::new(
        "management mode to acquire Shift",
        |state: &Observation| state.jiggling,
    ))?;
    let clicked = story.session().click(x, y, Button::Primary)?;
    let _struck = story.reaction(clicked).until(Condition::new(
        "management click to enter flight",
        |state: &Observation| state.flight == Flight::Striking,
    ))?;
    let shift_up = story.session().key_up(Key::Shift)?;
    let _stilled = story.reaction(shift_up).until(Condition::new(
        "management mode to release Shift",
        |state: &Observation| !state.jiggling,
    ))?;
    let _landed = story.wait(Condition::new(
        "management click to leave flight",
        |state: &Observation| state.flight == Flight::Grounded,
    ))?;
    vacate_gallery(
        story,
        "management pointer to release gallery reconciliation",
    )?;
    Ok(())
}

fn select_and_return(
    testbed: &Testbed,
    story: &mut Story<'_, '_, Observation>,
    app: &Application<'_>,
    thread: &str,
    proof: &Path,
    roster: &Path,
) -> Result<()> {
    story.session().focus()?;
    let (strike_x, strike_y) = seize_card(story, thread, "resume")?;
    let selected = story.session().click(strike_x, strike_y, Button::Primary)?;
    let _armed = story.reaction(selected).until(Condition::new(
        "card strike to enter flight",
        |state: &Observation| state.flight == Flight::Striking,
    ))?;
    app.wait_until(
        Duration::from_secs(10),
        "Codex session to be resumed in a fresh Alacritty",
        || Ok(proof.is_file()),
    )?;
    app.wait_until(
        Duration::from_secs(8),
        "Wrangler to conceal itself after resuming a Codex session",
        || Ok(wrangler_count(testbed)? == Some(0)),
    )?;
    app.wait_until(
        Duration::from_secs(8),
        "resumed Codex session to finish its account binding",
        || Ok(read_roster(roster)?["sessions"][thread]["account"]["account"].is_string()),
    )?;
    let _returned = story.session().key(Key::Function(7))?;
    app.wait_until(
        Duration::from_secs(8),
        "i3 to return to the fixed Wrangler workspace after resume",
        || Ok(wrangler_count(testbed)? == Some(1)),
    )?;
    // Witnesses describe presented surfaces. The activation worker may finish
    // while i3 has the Wrangler workspace occluded, so Grounded cannot lawfully
    // appear until the gallery returns and presents another frame.
    let _landed = story.wait(Condition::new(
        "Codex session strike to leave flight",
        |state: &Observation| state.flight == Flight::Grounded,
    ))?;
    Ok(())
}

fn seize_card(
    story: &mut Story<'_, '_, Observation>,
    thread: &str,
    purpose: &'static str,
) -> Result<(i16, i16)> {
    const ATTEMPTS: usize = 3;
    let mut attempt = 1;

    loop {
        vacate_gallery(
            story,
            "pointer to vacate the gallery before card acquisition",
        )?;
        let point = story.anchor(CardTarget(Harness::Codex, thread))?.center();
        let moved = story.session().move_to(point.0, point.1)?;
        let sought = thread.to_owned();
        let description = format!("{purpose} card `{thread}` to acquire the pointer");
        let rejection = description.clone();
        let condition = Condition::diagnostic(description, move |state: &Observation| {
            hovered(state, Harness::Codex, &sought)
                .then_some(())
                .ok_or_else(|| {
                    format!(
                        "{rejection}; actual hover={:?}, flight={:?}",
                        state.hovered, state.flight
                    )
                })
        });
        let mut reaction = story.reaction(moved);
        match reaction
            .within(ReactionBudget::functional(Duration::from_secs(2)))
            .until(condition)
        {
            Ok(_) => {
                story.session().focus()?;
                return Ok(point);
            }
            Err(TesterError::Condition { .. }) if attempt < ATTEMPTS => attempt += 1,
            Err(error) => return Err(error),
        }
    }
}

fn wait_card(
    story: &mut Story<'_, '_, Observation>,
    thread: &str,
    predicate: impl Fn(&CardObservation) -> bool,
) -> Result<CardObservation> {
    let thread = thread.to_owned();
    let sought = thread.clone();
    let frame = story.wait_stable(
        Duration::from_secs(10),
        Duration::from_millis(150),
        "Codex card to reach its expected lifecycle state",
        move |frame| {
            frame
                .state
                .cards
                .iter()
                .find(|card| card.thread == sought && predicate(card))
                .cloned()
        },
    )?;
    frame
        .state
        .cards
        .into_iter()
        .find(|card| card.thread == thread)
        .ok_or_else(|| TesterError::Verdict {
            detail: format!("expected lifecycle card `{thread}` vanished after stabilization"),
        })
}

fn read_roster(path: &Path) -> Result<Value> {
    serde_json::from_slice(&fs::read(path).map_err(io_verdict("read known-session state"))?)
        .map_err(|error| TesterError::Verdict {
            detail: format!("decode known-session state: {error}"),
        })
}

fn thread_archived(index: &Path, thread: &str) -> Option<bool> {
    Connection::open(index)
        .ok()?
        .query_row(
            "SELECT archived FROM threads WHERE id = ?1",
            params![thread],
            |row| row.get(0),
        )
        .ok()
}

fn thread_name(index: &Path, thread: &str) -> Option<String> {
    Connection::open(index)
        .ok()?
        .query_row(
            "SELECT name FROM threads WHERE id = ?1",
            params![thread],
            |row| row.get(0),
        )
        .ok()
}

fn legacy_thread_name(index: &Path, thread: &str) -> Option<String> {
    fs::read_to_string(index.parent()?.join("session_index.jsonl"))
        .ok()?
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|record| record.get("id").and_then(Value::as_str) == Some(thread))
        .filter_map(|record| {
            record
                .get("thread_name")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .next_back()
}

fn wrangler_count(testbed: &Testbed) -> Result<Option<usize>> {
    match testbed
        .x11()?
        .find_windows(&WindowQuery::title_exact("Codex Wrangler"))
    {
        Ok(windows) => Ok(Some(windows.len())),
        Err(TesterError::X11 { detail, .. }) if detail.contains("error_kind: Window") => Ok(None),
        Err(error) => Err(error),
    }
}

fn wait_named_window(
    testbed: &Testbed,
    app: &Application<'_>,
    title: &str,
    timeout: Duration,
) -> Result<Window> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match testbed
            .x11()?
            .wait_window_query(app, WindowQuery::title_exact(title), remaining)
        {
            Err(TesterError::X11 { detail, .. })
                if !remaining.is_zero() && detail.contains("error_kind: Window") =>
            {
                thread::sleep(Duration::from_millis(5));
            }
            result => return result,
        }
    }
}

fn verify_gallery(
    testbed: &Testbed,
    story: &mut Story<'_, '_, Observation>,
    fixture: &Fixture,
) -> Result<()> {
    vacate_gallery(
        story,
        "pointer to vacate the gallery before the initial snapshot",
    )?;
    let frame = story.wait_stable(
        Duration::from_secs(30),
        Duration::from_millis(250),
        "ten live terminals and one remembered Codex session",
        |frame| {
            (frame.state.cards.len() == 11
                && frame.state.cards.iter().all(|card| {
                    card.workspace
                        == if card.thread == DORMANT {
                            Some(6)
                        } else {
                            Some(7)
                        }
                }))
            .then(|| frame.state.cards.clone())
        },
    )?;
    verify_snapshot(&frame.state)?;
    verify_native_cursor_fields(story)?;
    enable_minimize_on_close(story, &fixture.configuration_path)?;
    verify_configuration_preflight(story, &fixture.configuration_path)?;
    verify_search_and_help(story)?;
    verify_history(testbed, story, &fixture.index)?;
    verify_permission_title_events(testbed, story)?;
    verify_workspace_badges(story)?;
    verify_fear_geometry(story)?;
    vacate_gallery(story, "pointer to vacate the gallery before state mutation")?;
    verify_goal_truth(story, &fixture.goals)?;
    verify_error_truth(story, &fixture.error_rollout)?;
    verify_hover_lock(story, &fixture.input_rollout)?;

    let turn = story.anchor(CardTarget(Harness::Codex, TURN))?;
    let (center_x, center_y) = turn.center();
    for (x, y, region) in [
        (center_x - 150, center_y - 75, "anonymous name"),
        (center_x - 150, center_y - 50, "working directory"),
        (center_x - 150, center_y - 20, "preview"),
        (center_x, center_y, "center"),
        (center_x + 170, center_y + 75, "empty corner"),
    ] {
        let receipt = story.session().move_to(x, y)?;
        let thread = TURN.to_owned();
        let _hovered = story
            .reaction(receipt)
            .within(input_reaction_budget())
            .until(Condition::new(
                format!("the entire tile hitbox to own its {region}"),
                move |state: &Observation| hovered(state, Harness::Codex, &thread),
            ))?;
    }

    vacate_gallery(story, "pointer to vacate every tile before capture")?;
    let _calm = story.wait_stable(
        Duration::from_secs(4),
        Duration::from_millis(500),
        "gallery water to settle before capture",
        |frame| frame.state.hovered.is_none().then_some(()),
    )?;

    let capture = testbed.private_path("captures/wrangler.png")?;
    story.capture()?.save_png(&capture)?;
    testbed.retain_on_failure("captures/wrangler.png")?;
    if let Some(destination) = env::var_os("CODEX_WRANGLER_ACCEPTANCE_CAPTURE") {
        fs::copy(&capture, destination).map_err(io_verdict("export acceptance capture"))?;
    }
    Ok(())
}

fn verify_native_cursor_fields(story: &mut Story<'_, '_, Observation>) -> Result<()> {
    vacate_gallery(
        story,
        "pointer to leave every tile before native cursor proof",
    )?;
    let rest = story.anchor(WorkspaceTarget(Harness::Codex, DONE))?.rect;
    let alt_down = story.session().key_down(Key::Alt)?;
    let _armed = story
        .reaction(alt_down)
        .within(input_reaction_budget())
        .until(Condition::new(
            "held Alt to arm the immobile Forge Pin field",
            |state: &Observation| {
                state.pin_field == PinField::Armed && !state.jiggling && state.hovered.is_none()
            },
        ))?;
    demand_native_cursor("Forge Pin", &ForgePin::cursor_image(), story.session())?;
    cross_card_with_native_cursor(story, DONE, "Forge Pin", &ForgePin::cursor_image())?;
    thread::sleep(Duration::from_millis(140));
    let _frame = story.frame()?;
    let pinning = story.anchor(WorkspaceTarget(Harness::Codex, DONE))?.rect;
    demand(
        rect_motion(rest, pinning) <= 0.05,
        "Alt made the tile move beneath its immobile Forge Pin field",
    )?;
    let alt_up = story.session().key_up(Key::Alt)?;
    let _released = story.reaction(alt_up).until(Condition::new(
        "released Alt to quench the Forge Pin field",
        |state: &Observation| state.pin_field == PinField::Quiescent,
    ))?;
    demand_native_cursor_released("Forge Pin", &ForgePin::cursor_image(), story.session())?;

    vacate_gallery(
        story,
        "pointer to leave every tile before Longinus precedence proof",
    )?;
    let longinus_rest = story.anchor(WorkspaceTarget(Harness::Codex, TURN))?.rect;
    let control_down = story.session().key_down(Key::Control)?;
    let _armed = story
        .reaction(control_down)
        .within(input_reaction_budget())
        .until(Condition::new(
            "held Ctrl to arm the Longinus fork field",
            |state: &Observation| {
                state.fork_field == ForkField::Armed && !state.jiggling && state.hovered.is_none()
            },
        ))?;
    demand_native_cursor("Longinus", &LonginusCursor::image(), story.session())?;
    cross_card_with_native_cursor(story, TURN, "Longinus", &LonginusCursor::image())?;
    thread::sleep(Duration::from_millis(140));
    let _frame = story.frame()?;
    let radiating = story.anchor(WorkspaceTarget(Harness::Codex, TURN))?.rect;
    demand(
        rect_motion(longinus_rest, radiating) <= 0.05,
        "Longinus radiator moved the tile beneath its waves",
    )?;
    let control_up = story.session().key_up(Key::Control)?;
    let _released = story.reaction(control_up).until(Condition::new(
        "released Ctrl to quench the Longinus fork field",
        |state: &Observation| state.fork_field == ForkField::Quiescent,
    ))?;
    demand_native_cursor_released("Longinus", &LonginusCursor::image(), story.session())?;
    vacate_gallery(story, "pointer to leave the native cursor fields")
}

fn cross_card_with_native_cursor(
    story: &mut Story<'_, '_, Observation>,
    thread: &str,
    label: &str,
    cursor: &CustomCursorImage,
) -> Result<()> {
    let point = story.anchor(CardTarget(Harness::Codex, thread))?.center();
    let moved = story.session().move_to(point.0, point.1)?;
    let sought = thread.to_owned();
    let _crossed = story
        .reaction(moved)
        .within(input_reaction_budget())
        .until(Condition::new(
            format!("held {label} cursor to cross into card `{thread}`"),
            move |state: &Observation| hovered(state, Harness::Codex, &sought),
        ))?;
    demand_native_cursor(label, cursor, story.session())
}

type CardSnapshot<'a> = BTreeSet<(Harness, &'a str, Work, Option<&'a str>, Option<u32>)>;

fn verify_snapshot(state: &Observation) -> Result<()> {
    demand(
        state.fingerprint == UI_FINGERPRINT,
        "Codex Wrangler witness fingerprint drifted",
    )?;
    demand(!state.loading, "settled snapshot still reports loading")?;
    demand(
        state.search.query.is_empty()
            && state.search.valid
            && !state.search.focused
            && !state.search.editing
            && state.guide == GuideVisibility::Closed
            && state.visible.len() == state.cards.len(),
        "settled gallery did not begin with an unfiltered, unfocused search",
    )?;
    demand(
        state.close_preference == ClosePreference::Exit,
        "minimize-on-close did not begin disabled",
    )?;
    let snapshot = state
        .cards
        .iter()
        .map(|card| {
            (
                card.harness,
                card.thread.as_str(),
                card.work,
                card.name.as_deref(),
                card.workspace,
            )
        })
        .collect::<CardSnapshot<'_>>();
    demand(
        snapshot == expected_card_snapshot(),
        format!("wrong card snapshot: {snapshot:?}"),
    )
}

fn expected_card_snapshot() -> CardSnapshot<'static> {
    BTreeSet::from([
        (
            Harness::Codex,
            ERROR,
            Work::Error,
            Some("Broken circuit"),
            Some(7),
        ),
        (
            Harness::Codex,
            INPUT,
            Work::Input,
            Some("Awaiting verdict"),
            Some(7),
        ),
        (
            Harness::Codex,
            GOAL,
            Work::Goal,
            Some("Violet frontier"),
            Some(7),
        ),
        (Harness::Codex, TURN, Work::Delegated, None, Some(7)),
        (
            Harness::Codex,
            DONE,
            Work::Done,
            Some("Silent machine"),
            Some(7),
        ),
        (Harness::Codex, PERMISSION, Work::Input, None, Some(7)),
        (
            Harness::Codex,
            ROTATE,
            Work::Done,
            Some("Old account"),
            Some(7),
        ),
        (
            Harness::Codex,
            FRESH,
            Work::Done,
            Some("Empty vessel"),
            Some(7),
        ),
        (
            Harness::Codex,
            DORMANT,
            Work::Closed,
            Some("Buried engine"),
            Some(6),
        ),
        (
            Harness::ClaudeCode,
            CLAUDE,
            Work::Done,
            Some("Copper invader"),
            Some(7),
        ),
        (
            Harness::PrimeAgent,
            PRIME,
            Work::Turn,
            Some("Butterfly engine"),
            Some(7),
        ),
    ])
}

fn enable_minimize_on_close(
    story: &mut Story<'_, '_, Observation>,
    configuration_path: &Path,
) -> Result<()> {
    let _enabled = story
        .tap(
            SettingTarget("minimize-on-close"),
            Button::Primary,
            Motion::default(),
        )?
        .within(input_reaction_budget())
        .until(Condition::new(
            "minimize-on-close latch to arm",
            |state: &Observation| state.close_preference == ClosePreference::Minimize,
        ))?;
    let _sealed = story.wait_stable(
        Duration::from_secs(5),
        Duration::from_millis(50),
        "minimize-on-close configuration settlement",
        |frame| (frame.state.settings.settled && configuration_path.is_file()).then_some(()),
    )?;
    let configuration =
        fs::read_to_string(configuration_path).map_err(io_verdict("read close setting"))?;
    demand(
        configuration
            .lines()
            .any(|line| line.trim() == "minimize_on_close = true"),
        "minimize-on-close did not seal its XDG configuration",
    )
}

fn verify_configuration_preflight(
    story: &mut Story<'_, '_, Observation>,
    configuration: &Path,
) -> Result<()> {
    let _settled = story.wait_stable(
        Duration::from_secs(5),
        Duration::from_millis(50),
        "configuration settlement",
        |frame| frame.state.settings.settled.then_some(()),
    )?;
    let admitted =
        fs::read_to_string(configuration).map_err(io_verdict("read admitted configuration"))?;
    let poisoned = format!("{admitted}\nunknown_preference = true\n");
    fs::write(configuration, &poisoned).map_err(io_verdict("poison configuration"))?;

    let opened = story.session().key(Key::Function(2))?;
    let _fault = story
        .reaction(opened)
        .within(input_reaction_budget())
        .until(Condition::new(
            "unknown configuration key to raise settings preflight",
            |state: &Observation| state.settings.open && state.settings.fault,
        ))?;
    demand(
        fs::read_to_string(configuration).is_ok_and(|source| source == poisoned),
        "configuration preflight modified the invalid source",
    )?;

    fs::write(configuration, admitted).map_err(io_verdict("repair configuration"))?;
    let _reloaded = story
        .tap(
            "eternalist.settings.reload",
            Button::Primary,
            Motion::default(),
        )?
        .within(input_reaction_budget())
        .until(Condition::new(
            "repaired configuration to reload",
            |state: &Observation| {
                state.settings.open && !state.settings.fault && state.settings.settled
            },
        ))?;
    let closed = story
        .session()
        .chord(Modifiers::CTRL, Key::Character(','))?;
    let _closed = story
        .reaction(closed)
        .within(input_reaction_budget())
        .until(Condition::new(
            "settings shortcut to close the repaired preflight",
            |state: &Observation| !state.settings.open,
        ))?;
    Ok(())
}

fn verify_search_and_help(story: &mut Story<'_, '_, Observation>) -> Result<()> {
    const QUERY: &str = "awaiting verdict|^/TEST/WORK/TURN$";

    story.session().focus()?;
    let slash = story.session().key(Key::Character('/'))?;
    let _focused = story
        .reaction(slash)
        .within(input_reaction_budget())
        .until(Condition::new(
            "slash to focus title search without entering itself",
            |state: &Observation| {
                state.search.editing && state.search.focused && state.search.query.is_empty()
            },
        ))?;
    let _editor = story.anchor(SearchTarget::Editor)?;

    let _filtered = story
        .type_text(QUERY)?
        .within(input_reaction_budget())
        .until(Condition::new(
            "regexp to match a named title and one anonymous path",
            |state: &Observation| {
                let visible = state
                    .visible
                    .iter()
                    .map(|card| (card.harness, card.thread.as_str()))
                    .collect::<BTreeSet<_>>();
                state.search.query == QUERY
                    && state.search.valid
                    && state.search.editing
                    && visible == BTreeSet::from([(Harness::Codex, INPUT), (Harness::Codex, TURN)])
            },
        ))?;

    let enter = story.session().key(Key::Return)?;
    let _compacted = story
        .reaction(enter)
        .within(input_reaction_budget())
        .until(Condition::new(
            "Enter to collapse the editor while preserving its filter",
            |state: &Observation| {
                state.search.query == QUERY
                    && state.search.valid
                    && !state.search.editing
                    && !state.search.focused
            },
        ))?;
    let _filter = story.anchor(SearchTarget::Filter)?;

    let slash = story.session().key(Key::Character('/'))?;
    let _refocused = story
        .reaction(slash)
        .within(input_reaction_budget())
        .until(Condition::new(
            "slash to reopen the active filter for editing",
            |state: &Observation| state.search.editing && state.search.focused,
        ))?;

    let _fail_open = story
        .replace_focused_text("[")?
        .within(input_reaction_budget())
        .until(Condition::new(
            "invalid regexp to remain explicit without erasing the roster",
            |state: &Observation| !state.search.valid && state.visible.len() == state.cards.len(),
        ))?;

    let escape = story.session().key(Key::Escape)?;
    let _cleared = story
        .reaction(escape)
        .within(input_reaction_budget())
        .until(Condition::new(
            "Escape to clear search without concealing Wrangler",
            |state: &Observation| {
                state.search.query.is_empty()
                    && state.search.valid
                    && !state.search.focused
                    && !state.search.editing
                    && state.visible.len() == state.cards.len()
            },
        ))?;

    let question = story.session().key(Key::Character('?'))?;
    let _opened = story
        .reaction(question)
        .within(input_reaction_budget())
        .until(Condition::new(
            "question mark to open generated command help",
            |state: &Observation| state.guide == GuideVisibility::Open,
        ))?;
    let close = story.session().key(Key::Escape)?;
    let _closed = story
        .reaction(close)
        .within(input_reaction_budget())
        .until(Condition::new(
            "Escape to close generated command help without concealing Wrangler",
            |state: &Observation| {
                state.guide == GuideVisibility::Closed && state.search.query.is_empty()
            },
        ))?;
    Ok(())
}

fn verify_history(
    testbed: &Testbed,
    story: &mut Story<'_, '_, Observation>,
    index: &Path,
) -> Result<()> {
    story.session().focus()?;
    let opened = story.session().key(Key::Tab)?;
    let _opened = story.reaction(opened).until(Condition::new(
        "physical Tab to open Historical",
        |state: &Observation| state.tab == Tab::Historical,
    ))?;
    let _indexed = story.wait_stable(
        Duration::from_secs(12),
        Duration::from_millis(250),
        "disk history to be counted and partitioned from Live",
        |frame| {
            let ids = frame
                .state
                .history
                .iter()
                .map(|session| session.thread.as_str())
                .collect::<BTreeSet<_>>();
            (ids == BTreeSet::from([UNSEEN, COLD])
                && frame.state.history.iter().all(|session| session.bytes > 0)
                && frame
                    .state
                    .history
                    .iter()
                    .find(|session| session.thread == UNSEEN)
                    .is_some_and(|session| session.turns == Some(2))
                && frame
                    .state
                    .history
                    .iter()
                    .find(|session| session.thread == COLD)
                    .is_some_and(|session| session.archived && session.turns == Some(1)))
            .then_some(())
        },
    )?;
    verify_history_rename(story, index)?;
    verify_archived_history_rename(story, index)?;
    verify_history_sorting(story)?;
    let capture = testbed.private_path("captures/wrangler-history.png")?;
    story.capture()?.save_png(&capture)?;
    testbed.retain_on_failure("captures/wrangler-history.png")?;
    if let Some(destination) = env::var_os("CODEX_WRANGLER_HISTORY_CAPTURE") {
        fs::copy(&capture, destination).map_err(io_verdict("export history capture"))?;
    }
    verify_history_archive_roundtrip(testbed, story, index)?;
    verify_history_deletion(story, index)?;

    let returned = story.session().key(Key::Tab)?;
    let _returned = story.reaction(returned).until(Condition::new(
        "physical Tab to return to Live",
        |state: &Observation| state.tab == Tab::Live,
    ))?;
    Ok(())
}

fn verify_history_rename(story: &mut Story<'_, '_, Observation>, index: &Path) -> Result<()> {
    let _opened = story
        .tap(
            HistoryTarget(UNSEEN, "rename"),
            Button::Primary,
            Motion::default(),
        )?
        .within(input_reaction_budget())
        .until(Condition::new(
            "historical pencil to arm its in-situ name editor",
            |state: &Observation| {
                state.history_rename.as_ref().is_some_and(|rename| {
                    rename.thread == UNSEEN && rename.draft == "Dust ledger" && rename.focused
                })
            },
        ))?;
    let cancelled = story.session().key(Key::Escape)?;
    let _cancelled = story
        .reaction(cancelled)
        .within(input_reaction_budget())
        .until(Condition::new(
            "Escape to cancel the historical name editor",
            |state: &Observation| state.history_rename.is_none(),
        ))?;
    let _reopened = story
        .tap(
            HistoryTarget(UNSEEN, "rename"),
            Button::Primary,
            Motion::default(),
        )?
        .within(input_reaction_budget())
        .until(Condition::new(
            "historical pencil to restore the cancelled editor",
            |state: &Observation| {
                state
                    .history_rename
                    .as_ref()
                    .is_some_and(|rename| rename.thread == UNSEEN && rename.focused)
            },
        ))?;
    let _typed = story
        .replace_text(
            HistoryTarget(UNSEEN, "rename-field"),
            RENAMED_HISTORY,
            Condition::new(
                "historical name editor to retain focus",
                |state: &Observation| {
                    state
                        .history_rename
                        .as_ref()
                        .is_some_and(|rename| rename.thread == UNSEEN && rename.focused)
                },
            ),
        )?
        .next_frame()?;
    let committed = story.session().key(Key::Return)?;
    let _committed = story
        .reaction(committed)
        .within(input_reaction_budget())
        .until(Condition::new(
            "Enter to submit the historical session name",
            |state: &Observation| state.history_rename.is_none(),
        ))?;
    let _latched = story.wait_stable(
        Duration::from_secs(10),
        Duration::from_millis(150),
        "successful rename to remain inert until authoritative snapshot reconciliation",
        |frame| {
            (thread_name(index, UNSEEN).as_deref() == Some(RENAMED_HISTORY)
                && frame
                    .state
                    .history
                    .iter()
                    .find(|session| session.thread == UNSEEN)
                    .is_some_and(|session| session.pending == Some(HistoryOperation::Rename)))
            .then_some(())
        },
    )?;
    vacate_history(story)?;
    let _renamed = story.wait_stable(
        Duration::from_secs(10),
        Duration::from_millis(120),
        "Codex metadata rename to return through the historical index",
        |frame| {
            frame
                .state
                .history
                .iter()
                .find(|session| session.thread == UNSEEN)
                .filter(|session| session.name.as_deref() == Some(RENAMED_HISTORY))
                .map(|_| ())
        },
    )?;
    demand(
        thread_name(index, UNSEEN).as_deref() == Some(RENAMED_HISTORY),
        "rename witness advanced before canonical Codex metadata",
    )?;
    demand(
        legacy_thread_name(index, UNSEEN).as_deref() == Some(RENAMED_HISTORY),
        "Codex rename omitted its legacy session-name projection",
    )
}

fn verify_archived_history_rename(
    story: &mut Story<'_, '_, Observation>,
    index: &Path,
) -> Result<()> {
    let _opened = story
        .tap(
            HistoryTarget(COLD, "rename"),
            Button::Primary,
            Motion::default(),
        )?
        .within(input_reaction_budget())
        .until(Condition::new(
            "archived pencil to arm its in-situ name editor",
            |state: &Observation| {
                state.history_rename.as_ref().is_some_and(|rename| {
                    rename.thread == COLD && rename.draft.is_empty() && rename.focused
                })
            },
        ))?;
    let _typed = story
        .replace_text(
            HistoryTarget(COLD, "rename-field"),
            RENAMED_ARCHIVED_HISTORY,
            Condition::new(
                "archived name editor to retain focus",
                |state: &Observation| {
                    state
                        .history_rename
                        .as_ref()
                        .is_some_and(|rename| rename.thread == COLD && rename.focused)
                },
            ),
        )?
        .next_frame()?;
    let committed = story.session().key(Key::Return)?;
    let _committed = story
        .reaction(committed)
        .within(input_reaction_budget())
        .until(Condition::new(
            "Enter to submit the archived session name",
            |state: &Observation| state.history_rename.is_none(),
        ))?;
    vacate_history(story)?;
    let _renamed = story.wait_stable(
        Duration::from_secs(10),
        Duration::from_millis(120),
        "archived metadata rename to return through the historical index",
        |frame| {
            frame
                .state
                .history
                .iter()
                .find(|session| session.thread == COLD)
                .filter(|session| {
                    session.archived && session.name.as_deref() == Some(RENAMED_ARCHIVED_HISTORY)
                })
                .map(|_| ())
        },
    )?;
    demand(
        thread_name(index, COLD).as_deref() == Some(RENAMED_ARCHIVED_HISTORY),
        "archived rename witness advanced before canonical Codex metadata",
    )?;
    click_history(story, COLD, "unarchive")?;
    vacate_history(story)?;
    let _unarchived = story.wait_stable(
        Duration::from_secs(10),
        Duration::from_millis(120),
        "renamed session to retain its name through unarchive",
        |frame| {
            (thread_archived(index, COLD) == Some(false)
                && thread_name(index, COLD).as_deref() == Some(RENAMED_ARCHIVED_HISTORY)
                && frame.state.history.iter().any(|session| {
                    session.thread == COLD
                        && !session.archived
                        && session.name.as_deref() == Some(RENAMED_ARCHIVED_HISTORY)
                }))
            .then_some(())
        },
    )?;
    click_history(story, COLD, "archive")?;
    vacate_history(story)?;
    let _rearchived = story.wait_stable(
        Duration::from_secs(10),
        Duration::from_millis(120),
        "renamed session to retain its name through rearchive",
        |frame| {
            (thread_archived(index, COLD) == Some(true)
                && thread_name(index, COLD).as_deref() == Some(RENAMED_ARCHIVED_HISTORY)
                && frame.state.history.iter().any(|session| {
                    session.thread == COLD
                        && session.archived
                        && session.name.as_deref() == Some(RENAMED_ARCHIVED_HISTORY)
                }))
            .then_some(())
        },
    )?;
    Ok(())
}

fn verify_history_sorting(story: &mut Story<'_, '_, Observation>) -> Result<()> {
    sort_history(
        story,
        HistoryColumn::SessionId,
        vec![(HistoryColumn::SessionId, SortDirection::Ascending)],
        vec![UNSEEN, COLD],
        "session sort to ascend",
    )?;
    sort_history(
        story,
        HistoryColumn::SessionId,
        vec![(HistoryColumn::SessionId, SortDirection::Descending)],
        vec![COLD, UNSEEN],
        "session sort to descend",
    )?;
    sort_history(
        story,
        HistoryColumn::Turns,
        vec![
            (HistoryColumn::SessionId, SortDirection::Descending),
            (HistoryColumn::Turns, SortDirection::Ascending),
        ],
        vec![COLD, UNSEEN],
        "later stable sort to preserve the prior order within equal-turn runs",
    )?;
    sort_history(
        story,
        HistoryColumn::Turns,
        vec![
            (HistoryColumn::SessionId, SortDirection::Descending),
            (HistoryColumn::Turns, SortDirection::Descending),
        ],
        vec![UNSEEN, COLD],
        "turn sort to descend",
    )?;
    sort_history(
        story,
        HistoryColumn::Turns,
        vec![(HistoryColumn::SessionId, SortDirection::Descending)],
        vec![COLD, UNSEEN],
        "turn sort to switch off without disturbing the remaining key",
    )?;
    sort_history(
        story,
        HistoryColumn::SessionId,
        Vec::new(),
        vec![UNSEEN, COLD],
        "final sort to switch off and restore snapshot order",
    )
}

fn sort_history(
    story: &mut Story<'_, '_, Observation>,
    column: HistoryColumn,
    sorts: Vec<(HistoryColumn, SortDirection)>,
    order: Vec<&'static str>,
    description: &'static str,
) -> Result<()> {
    let target = story.anchor(HistorySortTarget(column))?;
    let click = story
        .session()
        .click(target.center().0, target.center().1, Button::Primary)?;
    let _sorted = story
        .reaction(click)
        .within(input_reaction_budget())
        .until(Condition::new(description, move |state: &Observation| {
            state
                .history_sorts
                .iter()
                .map(|sort| (sort.column, sort.direction))
                .eq(sorts.iter().copied())
                && state
                    .history_order
                    .iter()
                    .map(String::as_str)
                    .eq(order.iter().copied())
        }))?;
    Ok(())
}

fn verify_history_archive_roundtrip(
    testbed: &Testbed,
    story: &mut Story<'_, '_, Observation>,
    index: &Path,
) -> Result<()> {
    click_history(story, UNSEEN, "archive")?;
    vacate_history(story)?;
    let _archived = story.wait_stable(
        Duration::from_secs(10),
        Duration::from_millis(150),
        "historical session to archive and compress",
        |frame| {
            (thread_archived(index, UNSEEN) == Some(true)
                && frame
                    .state
                    .history
                    .iter()
                    .find(|session| session.thread == UNSEEN)
                    .is_some_and(|session| session.archived))
            .then_some(())
        },
    )?;
    verify_history_transcript(testbed, story)?;
    click_history(story, UNSEEN, "unarchive")?;
    vacate_history(story)?;
    let _unarchived = story.wait_stable(
        Duration::from_secs(10),
        Duration::from_millis(150),
        "historical session to unarchive from compressed storage",
        |frame| {
            (thread_archived(index, UNSEEN) == Some(false)
                && frame
                    .state
                    .history
                    .iter()
                    .find(|session| session.thread == UNSEEN)
                    .is_some_and(|session| !session.archived))
            .then_some(())
        },
    )?;
    Ok(())
}

fn verify_history_transcript(
    testbed: &Testbed,
    story: &mut Story<'_, '_, Observation>,
) -> Result<()> {
    click_history(story, UNSEEN, "inspect")?;
    let _last = story.wait_stable(
        Duration::from_secs(10),
        Duration::from_millis(120),
        "historical row to reveal its last turn from compressed storage",
        |frame| {
            frame
                .state
                .history_transcript
                .as_ref()
                .filter(|transcript| {
                    transcript.thread == UNSEEN
                        && transcript.cursor == Some(1)
                        && transcript.total == 2
                        && transcript.user.as_deref() == Some("What did it become?")
                        && transcript.model.as_deref() == Some("The final copper machine.")
                        && transcript.error.is_none()
                })
                .map(|_| ())
        },
    )?;
    let capture = testbed.private_path("captures/wrangler-history-transcript.png")?;
    story.capture()?.save_png(&capture)?;
    testbed.retain_on_failure("captures/wrangler-history-transcript.png")?;
    if let Some(destination) = env::var_os("CODEX_WRANGLER_TRANSCRIPT_CAPTURE") {
        fs::copy(&capture, destination).map_err(io_verdict("export transcript capture"))?;
    }
    click_history(story, UNSEEN, "previous-turn")?;
    let _first = story.wait(Condition::new(
        "back arrow to reveal the preceding user/model turn",
        |state: &Observation| {
            state.history_transcript.as_ref().is_some_and(|transcript| {
                transcript.cursor == Some(0)
                    && transcript.user.as_deref() == Some("What is this engine?")
                    && transcript.model.as_deref() == Some("A brass prototype.")
            })
        },
    ))?;
    click_history(story, UNSEEN, "next-turn")?;
    let _last_again = story.wait(Condition::new(
        "forward arrow to restore the newest turn",
        |state: &Observation| {
            state
                .history_transcript
                .as_ref()
                .is_some_and(|transcript| transcript.cursor == Some(1))
        },
    ))?;
    let closed = story.session().key(Key::Escape)?;
    let _closed = story.reaction(closed).until(Condition::new(
        "Escape to close the turn inspector",
        |state: &Observation| state.history_transcript.is_none(),
    ))?;
    Ok(())
}

fn verify_history_open(testbed: &Testbed, story: &mut Story<'_, '_, Observation>) -> Result<()> {
    story.session().focus()?;
    let opened = story.session().key(Key::Tab)?;
    let _opened = story.reaction(opened).until(Condition::new(
        "physical Tab to open Historical before resuming a session",
        |state: &Observation| state.tab == Tab::Historical,
    ))?;
    let proof = testbed.private_path(format!("resume-proof-{UNSEEN}"))?;
    click_history(story, UNSEEN, "open")?;
    story.session().application().wait_until(
        Duration::from_secs(10),
        "historical session to open in Alacritty",
        || Ok(proof.is_file()),
    )?;
    demand(
        fs::read_to_string(proof).is_ok_and(|proof| proof.trim() == "resume 9"),
        "historical session did not open on Wrangler's current workspace",
    )?;
    let _returned = story.session().key(Key::Function(9))?;
    let _visible = story.wait_stable(
        Duration::from_secs(8),
        Duration::from_millis(120),
        "i3 to return focus to Wrangler after opening history",
        |frame| (frame.state.tab == Tab::Historical).then_some(()),
    )?;
    let returned = story.session().key(Key::Tab)?;
    let _returned = story.reaction(returned).until(Condition::new(
        "physical Tab to return to Live after opening history",
        |state: &Observation| state.tab == Tab::Live,
    ))?;
    Ok(())
}

fn verify_history_deletion(story: &mut Story<'_, '_, Observation>, index: &Path) -> Result<()> {
    click_history(story, COLD, "delete")?;
    let _prompt = story.wait(Condition::new(
        "historical deletion to demand confirmation",
        |state: &Observation| state.delete_prompt.as_deref() == Some(COLD),
    ))?;
    let _settled = story.wait_stable(
        Duration::from_secs(2),
        Duration::from_millis(120),
        "deletion confirmation geometry to settle",
        |frame| (frame.state.delete_prompt.as_deref() == Some(COLD)).then_some(()),
    )?;
    click_history(story, COLD, "confirm-future")?;
    let _unguarded = story.wait(Condition::new(
        "future deletion confirmation to be disabled",
        |state: &Observation| state.delete_guard == DeleteGuard::Bypassed,
    ))?;
    let escape = story.session().key(Key::Escape)?;
    let _cancelled = story.reaction(escape).until(Condition::new(
        "Escape to dismiss the armed deletion",
        |state: &Observation| state.delete_prompt.is_none(),
    ))?;
    click_history(story, COLD, "delete")?;
    let _latched = story.wait_stable(
        Duration::from_secs(10),
        Duration::from_millis(150),
        "successful deletion to remain inert beneath the pointer until snapshot reconciliation",
        |frame| {
            (thread_archived(index, COLD).is_none()
                && frame
                    .state
                    .history
                    .iter()
                    .find(|session| session.thread == COLD)
                    .is_some_and(|session| session.pending == Some(HistoryOperation::Delete)))
            .then_some(())
        },
    )?;
    vacate_history(story)?;
    let _deleted = story.wait_stable(
        Duration::from_secs(10),
        Duration::from_millis(150),
        "deleted session to leave disk history",
        |frame| {
            (!frame
                .state
                .history
                .iter()
                .any(|session| session.thread == COLD)
                && thread_archived(index, COLD).is_none())
            .then_some(())
        },
    )?;
    let _rearmed = story
        .tap(
            SettingTarget("confirm-delete"),
            Button::Primary,
            Motion::default(),
        )?
        .within(input_reaction_budget())
        .until(Condition::new(
            "delete confirmation setting to reach the application",
            |state: &Observation| state.tab == Tab::Historical,
        ))?;
    let _reguarded = story.wait(Condition::new(
        "delete confirmation to remain reversibly configurable",
        |state: &Observation| state.delete_guard == DeleteGuard::Armed,
    ))?;
    Ok(())
}

fn click_history(
    story: &mut Story<'_, '_, Observation>,
    thread: &str,
    action: &'static str,
) -> Result<()> {
    let _witnessed = story
        .tap(
            HistoryTarget(thread, action),
            Button::Primary,
            Motion::default(),
        )?
        .within(input_reaction_budget())
        .until(Condition::new(
            "historical control click to reach the application",
            |state: &Observation| state.tab == Tab::Historical,
        ))?;
    Ok(())
}

fn vacate_history(story: &mut Story<'_, '_, Observation>) -> Result<()> {
    let target = story.anchor(TabTarget(Tab::Historical))?;
    let _moved = story
        .session()
        .move_to(target.center().0, target.center().1)?;
    Ok(())
}

fn verify_workspace_badges(story: &mut Story<'_, '_, Observation>) -> Result<()> {
    for (harness, thread) in [
        (Harness::Codex, GOAL),
        (Harness::ClaudeCode, CLAUDE),
        (Harness::PrimeAgent, PRIME),
    ] {
        let card = story.anchor(CardTarget(harness, thread))?;
        let workspace = story.anchor(WorkspaceTarget(harness, thread))?;
        let [_card_left, card_top, card_right, _card_bottom] = card.rect;
        let [
            workspace_left,
            workspace_top,
            workspace_right,
            workspace_bottom,
        ] = workspace.rect;
        demand(
            (workspace_right - card_right).abs() <= 0.5 && (workspace_top - card_top).abs() <= 0.5,
            format!("{harness:?} workspace box escaped the card's top-right corner"),
        )?;
        demand(
            workspace_right - workspace_left >= 25.0 && workspace_bottom - workspace_top >= 25.0,
            format!("{harness:?} workspace box geometry collapsed"),
        )?;
    }
    Ok(())
}

fn verify_fear_geometry(story: &mut Story<'_, '_, Observation>) -> Result<()> {
    for thread in [ERROR, GOAL, TURN, INPUT, PERMISSION] {
        let running = story.anchor(CardTarget(Harness::Codex, thread))?;
        let _near = story
            .session()
            .move_to(running.center().0, running.center().1)?;
        let rest = story.anchor(WorkspaceTarget(Harness::Codex, thread))?.rect;
        let shift_down = story.session().key_down(Key::Shift)?;
        let _armed = story.reaction(shift_down).until(Condition::new(
            "management mode to arm beside running work",
            |state: &Observation| state.jiggling,
        ))?;
        thread::sleep(Duration::from_millis(140));
        let _frame = story.frame()?;
        let pose = story.anchor(WorkspaceTarget(Harness::Codex, thread))?.rect;
        demand(
            rect_motion(rest, pose) <= 0.05,
            format!("running tile {thread} was afraid of the management pointer"),
        )?;
        let shift_up = story.session().key_up(Key::Shift)?;
        let _stilled = story.reaction(shift_up).until(Condition::new(
            "management mode to release running work",
            |state: &Observation| !state.jiggling,
        ))?;
    }

    let done = story.anchor(CardTarget(Harness::Codex, DONE))?;
    let _near_done = story.session().move_to(done.center().0, done.center().1)?;
    let done_rest = story.anchor(WorkspaceTarget(Harness::Codex, DONE))?.rect;
    let closed_rest = story.anchor(WorkspaceTarget(Harness::Codex, DORMANT))?.rect;
    let shift_down = story.session().key_down(Key::Shift)?;
    let _armed = story.reaction(shift_down).until(Condition::new(
        "management mode to frighten a stopped tile",
        |state: &Observation| state.jiggling,
    ))?;
    let mut near_motion = 0.0_f32;
    let mut far_motion = 0.0_f32;
    for _sample in 0..4 {
        thread::sleep(Duration::from_millis(70));
        let _frame = story.frame()?;
        near_motion = near_motion.max(rect_motion(
            done_rest,
            story.anchor(WorkspaceTarget(Harness::Codex, DONE))?.rect,
        ));
        far_motion = far_motion.max(rect_motion(
            closed_rest,
            story.anchor(WorkspaceTarget(Harness::Codex, DORMANT))?.rect,
        ));
    }
    let shift_up = story.session().key_up(Key::Shift)?;
    let _stilled = story.reaction(shift_up).until(Condition::new(
        "management mode to release the stopped tile",
        |state: &Observation| !state.jiggling,
    ))?;
    demand(
        near_motion > 0.1 && near_motion > far_motion,
        format!("fear did not decay with pointer distance: near={near_motion}, far={far_motion}"),
    )
}

fn rect_motion(rest: [f32; 4], pose: [f32; 4]) -> f32 {
    rest.into_iter()
        .zip(pose)
        .map(|(rest, pose)| (pose - rest).abs())
        .fold(0.0, f32::max)
}

fn hovered(state: &Observation, harness: Harness, thread: &str) -> bool {
    state.hovered.as_ref()
        == Some(&CardKey {
            harness,
            thread: thread.to_owned(),
        })
}

fn verify_goal_truth(story: &mut Story<'_, '_, Observation>, path: &Path) -> Result<()> {
    let goals = Connection::open(path).map_err(verdict("open fixture goal ledger"))?;
    let _changed = goals
        .execute(
            "UPDATE thread_goals SET status = 'complete' WHERE thread_id = ?1",
            params![GOAL],
        )
        .map_err(verdict("complete fixture goal"))?;
    wait_for_work(story, GOAL, Work::Turn, "completed goal to turn green")?;

    let _deleted = goals
        .execute(
            "DELETE FROM thread_goals WHERE thread_id = ?1",
            params![GOAL],
        )
        .map_err(verdict("clear fixture goal"))?;
    wait_for_work(story, GOAL, Work::Turn, "cleared goal to remain green")?;

    let _inserted = goals
        .execute(
            "INSERT INTO thread_goals (thread_id, status) VALUES (?1, 'active')",
            params![GOAL],
        )
        .map_err(verdict("reactivate fixture goal"))?;
    wait_for_work(story, GOAL, Work::Goal, "active goal to turn violet")
}

fn verify_error_truth(story: &mut Story<'_, '_, Observation>, rollout: &Path) -> Result<()> {
    append_rollout(
        rollout,
        r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
    )?;
    wait_for_work(
        story,
        ERROR,
        Work::Turn,
        "a new turn to clear the prior error",
    )?;
    append_rollout(
        rollout,
        r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":null,"error":{"message":"A future Codex halt.","codex_error_info":"future_halt"}}}"#,
    )?;
    wait_for_work(
        story,
        ERROR,
        Work::Error,
        "an unknown error code to fail closed as error",
    )
}

fn verify_permission_title_events(
    testbed: &Testbed,
    story: &mut Story<'_, '_, Observation>,
) -> Result<()> {
    set_terminal_title(
        testbed,
        "[ ! ] Action Required | Permission Codex",
        "[ * ] Working | Permission Codex",
    )?;
    wait_for_work(
        story,
        PERMISSION,
        Work::Turn,
        "working terminal title to clear the permission wait",
    )?;
    set_terminal_title(
        testbed,
        "[ ! ] Action Required | Permission Codex",
        "[ ! ] Action Required | Permission Codex",
    )?;
    wait_for_work(
        story,
        PERMISSION,
        Work::Input,
        "action-required terminal title to demand input immediately",
    )
}

fn set_terminal_title(testbed: &Testbed, selector: &str, title: &str) -> Result<()> {
    let xprop = testbed.launch(
        AppCommand::new("/usr/bin/xprop")
            .args([
                "-name",
                selector,
                "-format",
                "_NET_WM_NAME",
                "8s",
                "-set",
                "_NET_WM_NAME",
                title,
            ])
            .runtime(Duration::from_secs(3)),
    )?;
    let exit = xprop.wait(Duration::from_secs(3))?;
    demand(
        exit.success(),
        format!("could not set fixture terminal `{selector}` title to `{title}`: {exit:?}"),
    )
}

fn verify_hover_lock(story: &mut Story<'_, '_, Observation>, rollout: &Path) -> Result<()> {
    let _seized = seize_card(story, INPUT, "input hover lock")?;

    append_rollout(
        rollout,
        r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call_fixture"}}"#,
    )?;
    let _locked = story.wait_stable(
        Duration::from_secs(5),
        Duration::from_millis(1_200),
        "hovered tile to retain identity while fresh state changes its sort class",
        |frame| {
            (hovered(&frame.state, Harness::Codex, INPUT)
                && frame
                    .state
                    .cards
                    .iter()
                    .any(|card| card.thread == INPUT && card.work == Work::Input))
            .then_some(())
        },
    )?;

    vacate_gallery(story, "pointer to vacate its tile and release the snapshot")?;
    wait_for_work(
        story,
        INPUT,
        Work::Turn,
        "released snapshot to admit the resolved input",
    )?;

    append_rollout(
        rollout,
        r#"{"type":"response_item","payload":{"type":"function_call","name":"request_user_input","call_id":"call_fixture_2"}}"#,
    )?;
    wait_for_work(
        story,
        INPUT,
        Work::Input,
        "fixture input state to be restored",
    )
}

fn append_rollout(path: &Path, line: &str) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(io_verdict("open fixture rollout for append"))?;
    writeln!(file, "{line}").map_err(io_verdict("append fixture rollout event"))
}

fn vacate_gallery(story: &mut Story<'_, '_, Observation>, description: &'static str) -> Result<()> {
    let frame = story.wait_stable(
        Duration::from_secs(10),
        Duration::ZERO,
        "gallery to expose a live card before pointer vacancy",
        |frame| {
            frame
                .anchors
                .iter()
                .any(|anchor| anchor.name.starts_with(CardTarget::PREFIX))
                .then_some(())
        },
    )?;
    let card = frame
        .anchors
        .iter()
        .filter(|anchor| anchor.name.starts_with(CardTarget::PREFIX))
        .min_by(|left, right| left.rect[1].total_cmp(&right.rect[1]))
        .ok_or_else(|| TesterError::Verdict {
            detail: "gallery vacancy requires a live card anchor".to_owned(),
        })?;
    let (x, _) = card.center();
    // Witness rectangles already inhabit X11's i16 coordinate space; this
    // midpoint remains between the client edge and the first live card.
    #[allow(clippy::cast_possible_truncation)]
    let y = (card.rect[1] / 2.0).round() as i16;
    let receipt = story.session().move_to(x, y)?;
    demand(
        story.session().pointer()? == (x, y),
        format!("gallery vacancy motion did not reach ({x}, {y})"),
    )?;
    let _vacated = story.reaction(receipt).until(Condition::diagnostic(
        description,
        move |state: &Observation| {
            state.hovered.as_ref().map_or(Ok(()), |hovered| {
                Err(format!(
                    "pointer at ({x}, {y}) retained card hover {hovered:?}"
                ))
            })
        },
    ))?;
    Ok(())
}

fn wait_for_work(
    story: &mut Story<'_, '_, Observation>,
    thread: &str,
    work: Work,
    description: &'static str,
) -> Result<()> {
    let _frame = story.wait_stable(
        Duration::from_secs(8),
        Duration::from_millis(150),
        description,
        |frame| {
            frame
                .state
                .cards
                .iter()
                .any(|card| card.thread == thread && card.work == work)
                .then_some(())
        },
    )?;
    Ok(())
}

struct Fixture {
    wrapper: PathBuf,
    focus_probe: PathBuf,
    desktop_recorder: PathBuf,
    posture_probe: PathBuf,
    goals: PathBuf,
    done_rollout: PathBuf,
    input_rollout: PathBuf,
    error_rollout: PathBuf,
    proof: PathBuf,
    workspace_proof: PathBuf,
    launch_workspace_proof: PathBuf,
    tiled_proof: PathBuf,
    tiled_away_proof: PathBuf,
    tiled_home_proof: PathBuf,
    floating_proof: PathBuf,
    state: PathBuf,
    roster: PathBuf,
    configuration_path: PathBuf,
    pinboard: PathBuf,
    index: PathBuf,
    rotate_resume: PathBuf,
    fork_launch: PathBuf,
    version_resume: PathBuf,
    dormant_resume: PathBuf,
}

impl Fixture {
    fn forge(testbed: &Testbed, binary: &Path) -> Result<Self> {
        let codex = testbed.create_private_dir("home/.codex/sessions/2026/08/03")?;
        let [goal, turn, done, input, permission, rotate, dormant, error] = seed_rollouts(&codex)?;
        let fresh = rollout(&codex, FRESH, "fresh");
        let _writer_locks = testbed.create_private_dir("home/.codex/thread-writer-locks")?;
        let archive = testbed.create_private_dir("home/.codex/archived_sessions")?;
        seed_historical(&codex, &archive)?;
        let db_path = testbed.private_path("home/.codex/state_5.sqlite")?;
        seed_index(&db_path)?;
        let goals = forge_goals(testbed)?;
        seed_names(testbed)?;
        let state = testbed.write_private("xdg/state/codex-wrangler/window-mode", b"tiled\n")?;
        let roster = seed_roster(testbed)?;
        forge_workdirs(testbed)?;
        let (claude, prime) = seed_foreign_transcripts(testbed)?;
        let _fake = forge_fake_harness(testbed)?;
        let _fake_app_server = forge_fake_app_server(testbed)?;
        let fake_cli = forge_fake_cli(testbed)?;
        forge_zstd_guard(testbed)?;
        let replaceable_alacritty = forge_replaceable_alacritty(testbed)?;
        let rotate_resume = testbed.private_path(format!("resume-proof-{ROTATE}"))?;
        let dormant_resume = testbed.private_path(format!("resume-proof-{DORMANT}"))?;
        let workspace_proof = testbed.private_path("workspace-proof")?;
        let launch_workspace_proof = testbed.private_path("launch-workspace-proof")?;
        let tiled_proof = testbed.private_path("tiled-proof")?;
        let tiled_away_proof = testbed.private_path("tiled-away-proof")?;
        let tiled_home_proof = testbed.private_path("tiled-home-proof")?;
        let floating_proof = testbed.private_path("floating-proof")?;
        let (focus_probe, desktop_recorder, posture_probe) = forge_desktop_probes(testbed)?;
        let i3 = testbed.write_private(
            "i3.config",
            "font pango:monospace 8\n\
             focus_follows_mouse no\n\
             workspace_layout tabbed\n\
             bindsym F10 workspace number 8, exec --no-startup-id touch /test/workspace-proof\n\
             bindsym F3 kill\n\
             bindsym F4 workspace number 9, exec --no-startup-id touch /test/launch-workspace-proof\n\
             bindsym F5 floating disable, exec --no-startup-id touch /test/tiled-proof\n\
             bindsym F6 workspace number 8, exec --no-startup-id touch /test/tiled-away-proof\n\
             bindsym F7 workspace number 9, exec --no-startup-id touch /test/tiled-home-proof\n\
             bindsym F8 floating enable, move position 0 0, exec --no-startup-id touch /test/floating-proof\n\
             bindsym F9 [title=\"^Codex Wrangler$\"] focus\n\
             bar {\n\
               mode dock\n\
               position bottom\n\
               tray_output screen\n\
             }\n",
        )?;
        let wrapper = forge_wrapper(
            testbed,
            binary,
            [
                &goal,
                &turn,
                &done,
                &input,
                &permission,
                &rotate,
                &claude,
                &prime,
                &error,
                &dormant,
                &fresh,
            ],
        )?;
        arm_executable(&wrapper, "make fixture wrapper executable")?;
        arm_executable(&fake_cli, "make fake Codex CLI executable")?;
        arm_executable(&focus_probe, "make focus probe executable")?;
        arm_executable(&desktop_recorder, "make desktop recorder executable")?;
        arm_executable(&posture_probe, "make posture probe executable")?;
        demand(
            replaceable_alacritty.is_file(),
            "replaceable Alacritty fixture was not created",
        )?;
        demand(i3.is_file(), "private i3 config was not created")?;
        Ok(Self {
            wrapper,
            focus_probe,
            desktop_recorder,
            posture_probe,
            goals,
            done_rollout: done,
            input_rollout: input,
            error_rollout: error,
            proof: testbed.private_path("focus-proof")?,
            workspace_proof,
            launch_workspace_proof,
            tiled_proof,
            tiled_away_proof,
            tiled_home_proof,
            floating_proof,
            state,
            roster,
            configuration_path: testbed.private_path("xdg/config/codex-wrangler/config.toml")?,
            pinboard: testbed.private_path("xdg/state/codex-wrangler/pinned-sessions.json")?,
            index: db_path,
            rotate_resume,
            fork_launch: testbed.private_path(format!("fork-proof-{TURN}"))?,
            version_resume: testbed.private_path(format!("resume-proof-{DONE}"))?,
            dormant_resume,
        })
    }
}

fn forge_goals(testbed: &Testbed) -> Result<PathBuf> {
    let goals = testbed.private_path("home/.codex/goals_1.sqlite")?;
    seed_goals(&goals)?;
    Ok(goals)
}

fn forge_workdirs(testbed: &Testbed) -> Result<()> {
    let _old_writer_lock =
        testbed.write_private(format!("home/.codex/thread-writer-locks/{FRESH}.lock"), b"")?;
    for work in [
        "turn",
        "rotate",
        "dormant",
        "done",
        "fresh-transplanted",
        "history",
    ] {
        let _work = testbed.create_private_dir(format!("work/{work}"))?;
    }
    let _git = testbed.create_private_dir("work/fresh-transplanted/.git")?;
    let _objects = testbed.create_private_dir("work/fresh-transplanted/.git/objects")?;
    let _heads = testbed.create_private_dir("work/fresh-transplanted/.git/refs/heads")?;
    let _head = testbed.write_private(
        "work/fresh-transplanted/.git/HEAD",
        b"ref: refs/heads/main\n",
    )?;
    let _config = testbed.write_private(
        "work/fresh-transplanted/.git/config",
        b"[core]\n\trepositoryformatversion = 0\n\tbare = false\n\
          [remote \"origin\"]\n\turl = fixture://transplanted\n",
    )?;
    Ok(())
}

fn forge_fake_harness(testbed: &Testbed) -> Result<PathBuf> {
    testbed.write_private(
        "fake-session.bash",
        br#"if [ "${1:-}" = resume ]; then
  shift
fi
rollout=$1
proof=$2
claim=${3:-$rollout}
if [ "$claim" = legacy ]; then
  claim=$rollout
fi
if [ "$claim" != none ]; then
  exec 9>>"$claim"
fi
if [ -n "${4:-}" ]; then
  exec 8>>"$4"
fi
case $(xprop -id "$WINDOWID" WM_CLASS) in
  *NeutralTerminal*) ;;
  *) exit 72 ;;
esac
xprop -id "$WINDOWID" -f _NET_WM_DESKTOP 32c -set _NET_WM_DESKTOP 0
IFS= read -r -n 1 key
[ -z "$key" ] || printf '%s\n' "$key" > "$proof"
sleep 90
"#,
    )
}

fn forge_fake_app_server(testbed: &Testbed) -> Result<PathBuf> {
    testbed.write_private(
        "fake-app-server.bash",
        br#"[ "$1" = app-server ] || exit 64
exec 9>>"$2"
sleep 90
"#,
    )
}

fn forge_replaceable_alacritty(testbed: &Testbed) -> Result<PathBuf> {
    let source = env::current_exe().map_err(io_verdict("locate terminal fixture executable"))?;
    let terminal = testbed.private_path("bin/alacritty-0.16.1-x11-ime")?;
    let replacement = testbed.private_path("bin/alacritty-0.16.1-x11-ime.next")?;
    let launcher = testbed.private_path("bin/alacritty")?;
    for destination in [&terminal, &replacement] {
        let _bytes = fs::copy(&source, destination)
            .map_err(io_verdict("copy replaceable terminal fixture"))?;
        fs::set_permissions(destination, fs::Permissions::from_mode(0o700))
            .map_err(io_verdict("make replaceable terminal fixture executable"))?;
    }
    symlink("alacritty-0.16.1-x11-ime", launcher)
        .map_err(io_verdict("link canonical fake Alacritty executable"))?;
    Ok(terminal)
}

fn forge_wrapper(testbed: &Testbed, binary: &Path, logs: [&Path; 11]) -> Result<PathBuf> {
    let names = logs.map(|path| path.file_name().unwrap_or_default().to_string_lossy());
    let wrapper = format!(
        "#!/bin/sh\n\
         export PATH=/test/bin:/usr/bin\n\
         terminal=/test/bin/alacritty-0.16.1-x11-ime\n\
         terminal_pids=\n\
         i3 -c /test/i3.config &\n\
         for attempt in $(seq 1 {I3_READINESS_POLLS}); do\n\
           i3-msg -t get_workspaces >/dev/null 2>&1 && break\n\
           sleep 0.{FIXTURE_POLL_INTERVAL_MILLIS:03}\n\
         done\n\
         i3-msg 'workspace number 7' >/dev/null\n\
         \"$terminal\" --class NeutralTerminal --title 'Goal Codex' -o 'window.position={{x=1500,y=0}}' -o 'window.dimensions={{columns=20,lines=5}}' -e bash -c \
           'exec -a codex bash /test/fake-session.bash /test/home/.codex/sessions/2026/08/03/{} /test/focus-proof legacy /test/home/.codex/sessions/2026/08/03/{}' &\n\
         terminal_pids=\"$terminal_pids $!\"\n\
         \"$terminal\" --class NeutralTerminal --title 'Turn Codex' -o 'window.position={{x=1500,y=200}}' -o 'window.dimensions={{columns=20,lines=5}}' -e bash -c \
           'exec -a codex bash /test/fake-session.bash /test/home/.codex/sessions/2026/08/03/{} /test/turn-proof /test/home/.codex/thread-writer-locks/{TURN}.lock' &\n\
         terminal_pids=\"$terminal_pids $!\"\n\
         \"$terminal\" --working-directory /test/work/fresh-transplanted --class NeutralTerminal --title 'Fresh Codex' -o 'window.position={{x=1500,y=300}}' -o 'window.dimensions={{columns=20,lines=5}}' -e bash -c \
           'exec -a codex bash /test/fake-session.bash resume /test/home/.codex/sessions/2026/08/03/{} /test/fresh-proof none' &\n\
         terminal_pids=\"$terminal_pids $!\"\n\
         \"$terminal\" --class NeutralTerminal --title 'Done Codex' -o 'window.position={{x=1500,y=400}}' -o 'window.dimensions={{columns=20,lines=5}}' -e bash -c \
           'exec -a codex bash /test/fake-session.bash /test/home/.codex/sessions/2026/08/03/{} /test/done-proof' &\n\
         terminal_pids=\"$terminal_pids $!\"\n\
         \"$terminal\" --class NeutralTerminal --title 'Input Codex' -o 'window.position={{x=1500,y=600}}' -o 'window.dimensions={{columns=20,lines=5}}' -e bash -c \
           'exec -a codex bash /test/fake-session.bash /test/home/.codex/sessions/2026/08/03/{} /test/input-proof' &\n\
         terminal_pids=\"$terminal_pids $!\"\n\
         \"$terminal\" --class NeutralTerminal --title '[ ! ] Action Required | Permission Codex' -o 'window.position={{x=1500,y=800}}' -o 'window.dimensions={{columns=20,lines=5}}' -e bash -c \
           'exec -a codex bash /test/fake-session.bash /test/home/.codex/sessions/2026/08/03/{} /test/permission-proof' &\n\
         terminal_pids=\"$terminal_pids $!\"\n\
         \"$terminal\" --class NeutralTerminal --title 'Old Account Codex' -o 'window.position={{x=1500,y=1000}}' -o 'window.dimensions={{columns=20,lines=5}}' -e bash -c \
           'exec -a codex bash /test/fake-session.bash /test/home/.codex/sessions/2026/08/03/{} /test/rotate-proof' &\n\
         terminal_pids=\"$terminal_pids $!\"\n\
         \"$terminal\" --class NeutralTerminal --title 'Claude Code' -o 'window.position={{x=1500,y=800}}' -o 'window.dimensions={{columns=20,lines=5}}' -e bash -c \
           'exec -a claude bash /test/fake-session.bash /test/home/.claude/projects/-work-claude/{} /test/claude-proof' &\n\
         terminal_pids=\"$terminal_pids $!\"\n\
         \"$terminal\" --class NeutralTerminal --title 'Prime Agent' -o 'window.position={{x=1500,y=1000}}' -o 'window.dimensions={{columns=20,lines=5}}' -e bash -c \
           'exec -a prime-agent bash /test/fake-session.bash /test/home/.prime/agent/sessions/{} /test/prime-proof' &\n\
         terminal_pids=\"$terminal_pids $!\"\n\
         \"$terminal\" --class NeutralTerminal --title 'Error Codex' -o 'window.position={{x=1500,y=1200}}' -o 'window.dimensions={{columns=20,lines=5}}' -e bash -c \
           'exec -a codex bash /test/fake-session.bash /test/home/.codex/sessions/2026/08/03/{} /test/error-proof' &\n\
         terminal_pids=\"$terminal_pids $!\"\n\
         fixture_windows=0\n\
         ready=0\n\
         for attempt in $(seq 1 {TERMINAL_READINESS_POLLS}); do\n\
           fixture_windows=$(i3-msg -t get_tree 2>/dev/null | jq '[.. | .window? | select(. != null)] | length')\n\
           ready=1\n\
           [ \"$fixture_windows\" -ge 10 ] || ready=0\n\
           for pid in $terminal_pids; do\n\
             case $(readlink \"/proc/$pid/exe\" 2>/dev/null) in\n\
               */alacritty-0.16.1-x11-ime) ;;\n\
               *) ready=0; break ;;\n\
             esac\n\
           done\n\
           [ \"$ready\" = 1 ] && break\n\
           sleep 0.{FIXTURE_POLL_INTERVAL_MILLIS:03}\n\
         done\n\
         if [ \"$ready\" != 1 ]; then\n\
           printf 'fixture mapped %s of 10 terminal windows\\n' \"$fixture_windows\" >&2\n\
           i3-msg -t get_tree | jq -c '[.. | objects | select(.window? != null) | .name]' >&2\n\
           exit 70\n\
         fi\n\
         bash -c 'exec -a codex bash /test/fake-app-server.bash app-server /test/home/.codex/thread-writer-locks/{FRESH}.lock' &\n\
         mv -f \"$terminal.next\" \"$terminal\"\n\
         for pid in $terminal_pids; do\n\
           case $(readlink \"/proc/$pid/exe\" 2>/dev/null) in\n\
             *' (deleted)') ;;\n\
             *) exit 71 ;;\n\
           esac\n\
         done\n\
         exec {}\n",
        names[0],
        names[9],
        names[1],
        names[10],
        names[2],
        names[3],
        names[4],
        names[5],
        names[6],
        names[7],
        names[8],
        binary.display(),
    );
    testbed.write_private("launch-wrangler", wrapper)
}

fn forge_fake_cli(testbed: &Testbed) -> Result<PathBuf> {
    let _bin = testbed.create_private_dir("bin")?;
    testbed.write_private(
        "bin/codex",
        format!(
            r#"#!/bin/bash
db=/test/home/.codex/state_5.sqlite
sqlite() {{ sqlite3 -batch -cmd '.timeout 2000' "$db" "$@"; }}
if [ "${{1:-}}" = --version ]; then
  printf '%s\n' 'codex-cli 0.147.0'
  exit 0
fi
if [ "${{1:-}}" = app-server ]; then
  while IFS= read -r request; do
    id=$(printf '%s\n' "$request" | jq -r '.id // empty')
    method=$(printf '%s\n' "$request" | jq -r '.method')
    [ -z "$id" ] && continue
    case $method in
      initialize)
        printf '%s\n' '{{"id":'$id',"result":{{}}}}'
        ;;
      account/read)
        printf '%s\n' '{{"id":'$id',"result":{{"account":{{"type":"chatgpt","email":"new@example.invalid","planType":"pro"}},"requiresOpenaiAuth":true}}}}'
        ;;
      account/rateLimits/read)
        printf '%s\n' '{{"id":'$id',"result":{{"rateLimits":{{"limitId":"codex","primary":{{"usedPercent":1,"windowDurationMins":10080,"resetsAt":{NEW_RESET}}}}},"rateLimitsByLimitId":{{}}}}}}'
        ;;
      thread/name/set)
        thread=$(printf '%s\n' "$request" | jq -r '.params.threadId')
        name=$(printf '%s\n' "$request" | jq -r '.params.name')
        quoted=$(printf '%s' "$name" | sed "s/'/''/g")
        changed=$(sqlite "UPDATE threads SET name = '$quoted' WHERE id = '$thread' AND archived = 0; SELECT changes();")
        if [ "$changed" = 1 ]; then
          jq -nc --arg id "$thread" --arg name "$name" '{{id:$id,thread_name:$name}}' >> /test/home/.codex/session_index.jsonl
          printf '%s\n' '{{"id":'$id',"result":{{}}}}'
        else
          printf '%s\n' '{{"id":'$id',"error":{{"code":-32602,"message":"thread cannot be renamed"}}}}'
        fi
        ;;
      thread/archive)
        thread=$(printf '%s\n' "$request" | jq -r '.params.threadId')
        rollout=$(sqlite "SELECT rollout_path FROM threads WHERE id = '$thread'")
        destination=/test/home/.codex/archived_sessions/${{rollout##*/}}
        mv "$rollout" "$destination"
        sqlite "UPDATE threads SET archived = 1, rollout_path = '$destination' WHERE id = '$thread'"
        printf '%s\n' '{{"id":'$id',"result":{{}}}}'
        ;;
      *)
        printf '%s\n' '{{"id":'$id',"result":{{}}}}'
        ;;
    esac
  done
  exit 0
fi

operation=$1
thread=$2
case $operation in
  fork)
    workspace=$(i3-msg -t get_workspaces | jq -r '.[] | select(.focused).num')
    printf '%s\n' "$operation $workspace" > "/test/${{operation}}-proof-${{thread}}"
    exec -a codex bash -c 'sleep 90' wrangler-fork
    ;;
  archive)
    rollout=$(sqlite "SELECT rollout_path FROM threads WHERE id = '$thread'")
    destination=/test/home/.codex/archived_sessions/${{rollout##*/}}
    mv "$rollout" "$destination"
    sqlite "UPDATE threads SET archived = 1, rollout_path = '$destination' WHERE id = '$thread'"
    printf '%s\n' "$operation" > "/test/${{operation}}-proof-${{thread}}"
    exit 0
    ;;
  unarchive)
    rollout=$(sqlite "SELECT rollout_path FROM threads WHERE id = '$thread'")
    destination=/test/home/.codex/sessions/2026/08/03/${{rollout##*/}}
    mv "$rollout" "$destination"
    sqlite "UPDATE threads SET archived = 0, rollout_path = '$destination' WHERE id = '$thread'"
    printf '%s\n' "$operation" > "/test/${{operation}}-proof-${{thread}}"
    exit 0
    ;;
  delete)
    thread=$3
    rollout=$(sqlite "SELECT rollout_path FROM threads WHERE id = '$thread'")
    rm -f "$rollout"
    sqlite "DELETE FROM threads WHERE id = '$thread'"
    printf '%s\n' "$operation" > "/test/${{operation}}-proof-${{thread}}"
    exit 0
    ;;
esac
workspace=$(i3-msg -t get_workspaces | jq -r '.[] | select(.focused).num')
printf '%s\n' "$operation $workspace" > "/test/${{operation}}-proof-${{thread}}"
rollout=$(sqlite "SELECT rollout_path FROM threads WHERE id = '$thread'")
exec -a codex bash -c 'exec 9>>"$1"; sleep 90; :' wrangler-resume "$rollout"
"#,
        ),
    )
}

fn forge_zstd_guard(testbed: &Testbed) -> Result<()> {
    let zstd = testbed.write_private(
        "bin/zstd",
        r#"#!/bin/sh
decode=0
long=0
for argument in "$@"; do
  case $argument in
    -d|--decompress) decode=1 ;;
    --long=31) long=1 ;;
  esac
done
if [ "$decode" = 1 ] && [ "$long" != 1 ]; then
  printf '%s\n' 'Wrangler omitted long-window archive support' >&2
  exit 70
fi
exec /usr/bin/zstd "$@"
"#,
    )?;
    arm_executable(&zstd, "make guarded zstd fixture executable")
}

fn arm_executable(path: &Path, operation: &'static str) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(io_verdict(operation))
}

fn seed_foreign_transcripts(testbed: &Testbed) -> Result<(PathBuf, PathBuf)> {
    let claude_dir = testbed.create_private_dir("home/.claude/projects/-work-claude")?;
    let claude = claude_dir.join(format!("{CLAUDE}.jsonl"));
    fs::write(
        &claude,
        concat!(
            "{\"type\":\"ai-title\",\"sessionId\":\"50000000-0000-7000-8000-000000000005\",\"aiTitle\":\"Copper invader\"}\n",
            "{\"type\":\"user\",\"cwd\":\"/work/claude\",\"sessionId\":\"50000000-0000-7000-8000-000000000005\",\"message\":{\"role\":\"user\",\"content\":\"Invade the copper machine.\"}}\n",
            "{\"type\":\"assistant\",\"cwd\":\"/work/claude\",\"sessionId\":\"50000000-0000-7000-8000-000000000005\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"The invader is still.\"}],\"stop_reason\":\"end_turn\"}}\n"
        ),
    )
    .map_err(io_verdict("write Claude transcript"))?;

    let prime_dir = testbed.create_private_dir("home/.prime/agent/sessions")?;
    let prime = prime_dir.join(format!("{PRIME}.jsonl"));
    fs::write(
        &prime,
        concat!(
            "{\"type\":\"session\",\"version\":3,\"id\":\"60000000-0000-7000-8000-000000000006\",\"cwd\":\"/work/prime\"}\n",
            "{\"type\":\"session_info\",\"id\":\"name\",\"name\":\"Butterfly engine\"}\n",
            "{\"type\":\"message\",\"id\":\"prompt\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"Raise the butterfly engine.\"}]}}\n"
        ),
    )
    .map_err(io_verdict("write Prime Agent transcript"))?;
    Ok((claude, prime))
}

fn forge_desktop_probes(testbed: &Testbed) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let focus = testbed.write_private(
        "focus-probe",
        br#"#!/bin/sh
window=$1
fixed=${2:-}
expected=$(printf '0x%x' "$window")
for attempt in $(seq 1 200); do
  active=$(xprop -root _NET_ACTIVE_WINDOW 2>/dev/null | sed -n 's/.*# //p')
  current=$(xprop -root _NET_CURRENT_DESKTOP 2>/dev/null | sed -n 's/.*= //p')
  desktop=$(xprop -id "$window" _NET_WM_DESKTOP 2>/dev/null | sed -n 's/.*= //p')
  pinned=$desktop
  if [ -n "$fixed" ]; then
    pinned=$(cat "$fixed" 2>/dev/null)
  fi
  if [ "$active" = "$expected" ] && [ "$desktop" = "$current" ] && [ "$desktop" = "$pinned" ]; then
    exit 0
  fi
  sleep 0.02
done
printf 'window=%s expected=%s active=%s current=%s desktop=%s\n' \
  "$window" "$expected" "$active" "$current" "$desktop" >&2
exit 1
"#,
    )?;
    let recorder = testbed.write_private(
        "desktop-recorder",
        br#"#!/bin/sh
window=$1
destination=$2
for attempt in $(seq 1 200); do
  desktop=$(xprop -id "$window" _NET_WM_DESKTOP 2>/dev/null | sed -n 's/.*= //p')
  if [ -n "$desktop" ]; then
    printf '%s\n' "$desktop" > "$destination"
    exit 0
  fi
  sleep 0.02
done
exit 1
"#,
    )?;
    let posture = testbed.write_private(
        "posture-probe",
        br#"#!/bin/sh
window=$1
expected=$2
for attempt in $(seq 1 200); do
  tree=$(i3-msg -t get_tree 2>/dev/null)
  actual=$(printf '%s\n' "$tree" | jq -r \
    --argjson window "$window" '
      def sight($inherited):
        ((.floating == "auto_on") or (.floating == "user_on")) as $own
        | ($inherited or $own) as $here
        | if .window == $window then $here
          else (([.nodes[]? | sight($here)] + [.floating_nodes[]? | sight(true)])
            | map(select(. != null)) | if length > 0 then .[0] else null end)
          end;
      sight(false)
    ')
  if [ "$actual" = true ]; then
    actual=floating
  elif [ "$actual" = false ]; then
    actual=tiled
  fi
  if [ "$actual" = "$expected" ]; then
    exit 0
  fi
  sleep 0.02
done
printf 'window=%s expected=%s actual=%s\n' "$window" "$expected" "$actual" >&2
exit 1
"#,
    )?;
    Ok((focus, recorder, posture))
}

fn seed_goals(path: &Path) -> Result<()> {
    let db = Connection::open(path).map_err(verdict("create fixture goal ledger"))?;
    db.execute_batch(
        "CREATE TABLE thread_goals (
           thread_id TEXT PRIMARY KEY NOT NULL,
           status TEXT NOT NULL
         );",
    )
    .map_err(verdict("declare fixture goal ledger"))?;
    for (thread, status) in [
        (GOAL, "active"),
        (TURN, "complete"),
        (DONE, "complete"),
        (INPUT, "complete"),
    ] {
        db.execute(
            "INSERT INTO thread_goals (thread_id, status) VALUES (?1, ?2)",
            params![thread, status],
        )
        .map_err(verdict("seed fixture goal"))?;
    }
    Ok(())
}

fn seed_index(path: &Path) -> Result<()> {
    let db = Connection::open(path).map_err(verdict("create fixture thread index"))?;
    db.execute_batch(
        "CREATE TABLE threads (
           id TEXT PRIMARY KEY, title TEXT NOT NULL, name TEXT, cwd TEXT NOT NULL,
           updated_at_ms INTEGER NOT NULL, thread_source TEXT, source TEXT NOT NULL,
           agent_role TEXT, rollout_path TEXT NOT NULL,
           cli_version TEXT NOT NULL DEFAULT '0.147.0',
           git_origin_url TEXT,
           archived INTEGER NOT NULL DEFAULT 0
         );",
    )
    .map_err(verdict("declare fixture thread index"))?;
    seed_thread_rows(&db)?;
    db.execute(
        "UPDATE threads SET cli_version = '0.146.0' WHERE id = ?1",
        params![DONE],
    )
    .map_err(verdict("seed superseded Codex version"))?;
    db.execute(
        "UPDATE threads
         SET source = 'vscode', cli_version = '0.149.0',
             cwd = '/test/work/fresh-before-transplant',
             git_origin_url = 'fixture://transplanted'
         WHERE id = ?1",
        params![FRESH],
    )
    .map_err(verdict("seed contradictory 0.149 TUI provenance"))?;
    db.execute(
        "UPDATE threads SET archived = 1 WHERE id = ?1",
        params![DORMANT],
    )
    .map_err(verdict("oppose Codex archive state to Wrangler closure"))?;
    db.execute(
        "UPDATE threads SET archived = 1 WHERE id = ?1",
        params![COLD],
    )
    .map_err(verdict("archive cold historical session"))?;
    Ok(())
}

fn seed_thread_rows(db: &Connection) -> Result<()> {
    for row in [
        (
            GOAL,
            "Prompt-derived goal title",
            None::<&str>,
            "/work/goal",
            30_i64,
            rollout_test_path(GOAL, "goal"),
        ),
        (
            TURN,
            "This first prompt must never become the displayed name",
            None,
            "/test/work/turn",
            20,
            rollout_test_path(TURN, "turn"),
        ),
        (
            DONE,
            "Prompt-derived done title",
            None,
            "/test/work/done",
            10,
            rollout_test_path(DONE, "done"),
        ),
        (
            INPUT,
            "Prompt-derived input title",
            None,
            "/work/input",
            40,
            rollout_test_path(INPUT, "input"),
        ),
        (
            PERMISSION,
            "Prompt-derived permission title",
            None,
            "/work/permission",
            50,
            rollout_test_path(PERMISSION, "permission"),
        ),
        (
            ROTATE,
            "Prompt-derived rotated title",
            Some("Old account"),
            "/test/work/rotate",
            9,
            rollout_test_path(ROTATE, "rotate"),
        ),
        (
            DORMANT,
            "Prompt-derived dormant title",
            Some("Buried engine"),
            "/test/work/dormant",
            8,
            rollout_test_path(DORMANT, "dormant"),
        ),
        (
            ERROR,
            "Prompt-derived error title",
            Some("Broken circuit"),
            "/work/error",
            60,
            rollout_test_path(ERROR, "error"),
        ),
        (
            FRESH,
            "Fresh thread",
            Some("Empty vessel"),
            "/test/work/fresh",
            70,
            rollout_test_path(FRESH, "fresh"),
        ),
        (
            UNSEEN,
            "Historical thread",
            Some("Dust ledger"),
            "/test/work/history",
            7,
            rollout_test_path(UNSEEN, "unseen"),
        ),
        (
            COLD,
            "Cold historical thread",
            None,
            "/test/work/history",
            6,
            format!(
                "/test/home/.codex/archived_sessions/rollout-2026-08-03T00-00-00-cold-{COLD}.jsonl"
            ),
        ),
    ] {
        seed_thread(db, &row)?;
    }
    Ok(())
}

type ThreadSeed<'a> = (&'a str, &'a str, Option<&'a str>, &'a str, i64, String);

fn seed_thread(db: &Connection, row: &ThreadSeed<'_>) -> Result<()> {
    let _inserted = db
        .execute(
            "INSERT INTO threads
             (id, title, name, cwd, updated_at_ms, thread_source, source, agent_role,
              rollout_path, archived)
             VALUES (?1, ?2, ?3, ?4, ?5, 'user', 'cli', NULL, ?6, 0)",
            params![row.0, row.1, row.2, row.3, row.4, row.5],
        )
        .map_err(verdict("seed fixture thread"))?;
    Ok(())
}

fn seed_historical(sessions: &Path, archive: &Path) -> Result<()> {
    for (path, transcript) in [
        (
            rollout(sessions, UNSEEN, "unseen"),
            concat!(
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"item_completed\",\"item\":{\"type\":\"UserMessage\",\"content\":[{\"type\":\"text\",\"text\":\"What is this engine?\"}]}}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"item_completed\",\"item\":{\"type\":\"AgentMessage\",\"content\":[{\"type\":\"text\",\"text\":\"A brass prototype.\"}]}}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"turn_started\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"item_completed\",\"item\":{\"type\":\"UserMessage\",\"content\":[{\"type\":\"text\",\"text\":\"What did it become?\"}]}}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"item_completed\",\"item\":{\"type\":\"AgentMessage\",\"content\":[{\"type\":\"text\",\"text\":\"The final copper machine.\"}]}}}\n",
            ),
        ),
        (
            archive.join(format!("rollout-2026-08-03T00-00-00-cold-{COLD}.jsonl")),
            concat!(
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"First buried question.\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"First buried answer.\"}}\n",
            ),
        ),
    ] {
        fs::write(path, transcript).map_err(io_verdict("write historical rollout"))?;
    }
    Ok(())
}

fn seed_roster(testbed: &Testbed) -> Result<PathBuf> {
    let state = serde_json::json!({
        "version": 2,
        "sessions": {
            DORMANT: {
                "name": "Buried engine",
                "cwd": "/test/work/dormant",
                "preview": "Waiting below.",
                "updated_at_ms": 8,
                "workspace": 6,
                "account": {
                    "quotas": [{
                        "limit": "codex",
                        "window_minutes": 10_080,
                        "resets_at": OLD_RESET
                    }]
                }
            }
        }
    });
    let bytes = serde_json::to_vec(&state).map_err(|error| TesterError::Verdict {
        detail: format!("encode known-session fixture: {error}"),
    })?;
    testbed.write_private("xdg/state/codex-wrangler/known-sessions.json", bytes)
}

fn seed_names(testbed: &Testbed) -> Result<()> {
    let names = format!(
        concat!(
            "{{\"id\":\"{goal}\",\"thread_name\":\"Superseded name\"}}\n",
            "{{\"id\":\"{goal}\",\"thread_name\":\"Violet frontier\"}}\n",
            "{{\"id\":\"{done}\",\"thread_name\":\"Silent machine\"}}\n",
            "{{\"id\":\"{input}\",\"thread_name\":\"Awaiting verdict\"}}\n"
        ),
        goal = GOAL,
        done = DONE,
        input = INPUT
    );
    let index = testbed.write_private("home/.codex/session_index.jsonl", names)?;
    demand(index.is_file(), "session-name index was not created")
}

fn seed_rollouts(directory: &Path) -> Result<[PathBuf; 8]> {
    let goal = rollout(directory, GOAL, "goal");
    let turn = rollout(directory, TURN, "turn");
    let done = rollout(directory, DONE, "done");
    let input = rollout(directory, INPUT, "input");
    let permission = rollout(directory, PERMISSION, "permission");
    let rotate = rollout(directory, ROTATE, "rotate");
    let dormant = rollout(directory, DORMANT, "dormant");
    let error = rollout(directory, ERROR, "error");
    fs::write(
        &goal,
        concat!(
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"Pursue the violet frontier.\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_goal_updated\",\"goal\":{\"status\":\"active\"}}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n"
        ),
    )
    .map_err(io_verdict("write goal rollout"))?;
    fs::write(
        &turn,
        concat!(
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n",
            r#"{"type":"event_msg","payload":{"type":"item_completed","thread_id":"00000000-0000-0000-0000-000000000002","turn_id":"turn-fixture","item":{"type":"UserMessage","id":"user-fixture","client_id":"wire-peer/post-fixture","content":[{"type":"text","text":"Cut the delegated task through a deliberately immense preview which must remain imprisoned inside the card. It keeps going through several clauses, several sentences, and enough visual matter to expose any label whose clip rectangle is merely aspirational rather than real. None of this text may trespass into the next row, however narrow the window becomes.","text_elements":[]}]},"started_at_ms":1,"completed_at_ms":1}}"#,
            "\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_goal_updated\",\"goal\":{\"status\":\"active\"}}}\n"
        ),
    )
    .map_err(io_verdict("write delegated rollout"))?;
    fs::write(
        &done,
        concat!(
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_goal_updated\",\"goal\":{\"status\":\"active\"}}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"last_agent_message\":\"The machine is still.\"}}\n"
        ),
    )
    .map_err(io_verdict("write completed rollout"))?;
    fs::write(
        &input,
        concat!(
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"Request a verdict.\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"request_user_input\",\"call_id\":\"call_fixture\"}}\n"
        ),
    )
    .map_err(io_verdict("write input-wait rollout"))?;
    fs::write(
        &permission,
        concat!(
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"Request a permission.\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n"
        ),
    )
    .map_err(io_verdict("write permission-wait rollout"))?;
    let rotate_events = [
        serde_json::json!({
            "type": "event_msg",
            "payload": {"type": "user_message", "message": "Change the guard."}
        }),
        serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "rate_limits": {
                    "limit_id": "codex",
                    "primary": {"window_minutes": 10_080, "resets_at": OLD_RESET}
                }
            }
        }),
        serde_json::json!({
            "type": "event_msg",
            "payload": {"type": "task_complete", "last_agent_message": "The old guard sleeps."}
        }),
    ];
    let rotate_text = rotate_events
        .into_iter()
        .map(|event| event.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(&rotate, rotate_text).map_err(io_verdict("write account-rollover rollout"))?;
    fs::write(
        &dormant,
        concat!(
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"Wake the buried engine.\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"last_agent_message\":\"Waiting below.\"}}\n"
        ),
    )
    .map_err(io_verdict("write dormant rollout"))?;
    fs::write(
        &error,
        concat!(
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"Test an unknown failure.\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"last_agent_message\":null,\"error\":{\"message\":\"A future Codex halt.\",\"codex_error_info\":\"future_halt\"}}}\n"
        ),
    )
    .map_err(io_verdict("write error rollout"))?;
    Ok([goal, turn, done, input, permission, rotate, dormant, error])
}

fn rollout(directory: &Path, id: &str, stamp: &str) -> PathBuf {
    directory.join(format!("rollout-2026-08-03T00-00-00-{stamp}-{id}.jsonl"))
}

fn rollout_test_path(id: &str, stamp: &str) -> String {
    format!("/test/home/.codex/sessions/2026/08/03/rollout-2026-08-03T00-00-00-{stamp}-{id}.jsonl")
}

fn sibling_binary() -> Result<PathBuf> {
    let current = env::current_exe().map_err(io_verdict("locate acceptance executable"))?;
    let directory = current
        .parent()
        .ok_or_else(|| egui_tester::Error::Verdict {
            detail: "acceptance executable has no parent directory".to_owned(),
        })?;
    let binary = env::var_os("CODEX_WRANGLER_ACCEPTANCE_BINARY")
        .map_or_else(|| directory.join("codex-wrangler"), PathBuf::from);
    demand(
        binary.is_file(),
        format!("Codex Wrangler binary is absent at `{}`", binary.display()),
    )?;
    Ok(binary)
}

fn verdict(operation: &'static str) -> impl FnOnce(rusqlite::Error) -> egui_tester::Error {
    move |error| egui_tester::Error::Verdict {
        detail: format!("{operation}: {error}"),
    }
}

fn io_verdict(operation: &'static str) -> impl FnOnce(std::io::Error) -> egui_tester::Error {
    move |error| egui_tester::Error::Verdict {
        detail: format!("{operation}: {error}"),
    }
}

fn demand_native_cursor(
    label: &str,
    expected: &CustomCursorImage,
    session: &X11Session<'_, '_>,
) -> Result<()> {
    let expected_pixels = native_cursor_pixels(expected);
    let deadline = Instant::now() + input_reaction_budget().functional_timeout();
    loop {
        let actual = session.cursor_image()?;
        let first_pixel_delta = actual
            .argb
            .iter()
            .zip(&expected_pixels)
            .position(|(actual, expected)| actual != expected);
        if native_cursor_is(&actual, expected, &expected_pixels) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return demand(
                false,
                format!(
                    "X11 did not install the exact {label} cursor requested by Wrangler: actual {}x{} @ {},{} ({} pixels), expected {}x{} @ {},{} ({} pixels), first pixel delta {first_pixel_delta:?}",
                    actual.width,
                    actual.height,
                    actual.hotspot_x,
                    actual.hotspot_y,
                    actual.argb.len(),
                    expected.size[0],
                    expected.size[1],
                    expected.hotspot[0],
                    expected.hotspot[1],
                    expected_pixels.len(),
                ),
            );
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn demand_native_cursor_released(
    label: &str,
    expected: &CustomCursorImage,
    session: &X11Session<'_, '_>,
) -> Result<()> {
    let expected_pixels = native_cursor_pixels(expected);
    let deadline = Instant::now() + input_reaction_budget().functional_timeout();
    loop {
        let actual = session.cursor_image()?;
        if !native_cursor_is(&actual, expected, &expected_pixels) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return demand(
                false,
                format!(
                    "X11 retained the {label} cursor after its modifier was released without pointer motion"
                ),
            );
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn native_cursor_pixels(cursor: &CustomCursorImage) -> Vec<u32> {
    cursor
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .map(|pixel| {
            u32::from(pixel[3]) << 24
                | u32::from(pixel[0]) << 16
                | u32::from(pixel[1]) << 8
                | u32::from(pixel[2])
        })
        .collect()
}

fn native_cursor_is(
    actual: &X11CursorImage,
    expected: &CustomCursorImage,
    expected_pixels: &[u32],
) -> bool {
    actual.width == expected.size[0]
        && actual.height == expected.size[1]
        && actual.hotspot_x == expected.hotspot[0]
        && actual.hotspot_y == expected.hotspot[1]
        && actual.argb == expected_pixels
}
