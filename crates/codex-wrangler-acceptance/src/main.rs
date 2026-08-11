use std::{
    collections::BTreeSet,
    env, fs,
    io::Write as _,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use egui_tester::{
    AppCommand, Application, Button, Condition, Error as TesterError, Key, ReactionBudget, Result,
    Story, Testbed, TestbedBuilder, Window, WindowQuery, demand,
};
use rusqlite::{Connection, params};
use serde_json::Value;

#[path = "../../codex-wrangler/src/contract.rs"]
mod contract;
use contract::{
    CardKey, CardTarget, Flight, Harness, LogoTarget, Observation, UI_FINGERPRINT, Work,
    WorkspaceTarget,
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
const OLD_RESET: i64 = 1_000_000;
const NEW_RESET: i64 = 2_000_000;

fn main() -> Result<()> {
    let binary = sibling_binary()?;
    TestbedBuilder::default()
        .failure_artifacts("/tmp/codex-wrangler-acceptance-failure")
        .run(|testbed| story(testbed, &binary))
}

fn story(testbed: &Testbed, binary: &Path) -> Result<()> {
    let fixture = Fixture::forge(testbed, binary)?;
    let app = testbed.launch(
        AppCommand::new(&fixture.wrapper)
            .borrow_read_only(binary)
            .private_env("CODEX_HOME", "home/.codex")
            .private_env("CODEX_WRANGLER_TEST_TRAY_WINDOW", "1")
            .witness("probes/wrangler")
            .runtime(Duration::from_secs(90)),
    )?;
    let wrangler = wait_named_window(testbed, &app, "Codex Wrangler", Duration::from_secs(8))?;
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
    let _switched = story.session().key(Key::Function(2))?;
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

    verify_management_veto(story, fixture)?;

    select_and_return(testbed, story, app, ROTATE, &fixture.rotate_resume)?;
    demand(
        fs::read_to_string(&fixture.rotate_resume).is_ok_and(|proof| proof.trim() == "resume 7"),
        "rolled session did not resume on its terminal workspace",
    )?;
    app.wait_until(
        Duration::from_secs(8),
        "rolled session to bind the current Codex account",
        || Ok(read_roster(&fixture.roster)?["sessions"][ROTATE]["account"]["account"].is_string()),
    )?;
    demand(
        read_roster(&fixture.roster)?["sessions"][ROTATE]["account"]["account"].is_string(),
        "rolled session was not rebound to the current Codex account",
    )?;

    select_and_return(testbed, story, app, DORMANT, &fixture.dormant_resume)?;
    demand(
        fs::read_to_string(&fixture.dormant_resume).is_ok_and(|proof| proof.trim() == "resume 6"),
        "resurrected session did not return to its remembered workspace",
    )?;

    shift_click_card(story, DONE)?;
    let archived = wait_card(story, DONE, |card| !card.open && card.archived)?;
    demand(
        archived.work == Work::Done,
        "archived session acquired a spurious work state",
    )?;
    demand(
        thread_archived(&fixture.index, DONE) == Some(true)
            && fixture.archived_rollout.is_file()
            && read_roster(&fixture.roster)?["sessions"][DONE]["retention"] == "archived",
        "archive did not converge across Codex storage and Wrangler state",
    )?;

    shift_click_card(story, DONE)?;
    let _gone = story.wait_stable(
        Duration::from_secs(8),
        Duration::from_millis(150),
        "forgotten archive to leave the gallery",
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
            && thread_archived(&fixture.index, DONE) == Some(true)
            && fixture.archived_rollout.is_file(),
        "forgetting an archive mutated Codex storage or was not sealed immediately",
    )?;
    Ok(())
}

fn verify_management_veto(story: &mut Story<'_, '_, Observation>, fixture: &Fixture) -> Result<()> {
    let shift_down = story.session().key_down(Key::Shift)?;
    let _jiggling = story
        .reaction(shift_down)
        .within(ReactionBudget::performance(Duration::from_millis(50)))
        .until(Condition::new(
            "held Shift to animate management mode",
            |state: &Observation| state.jiggling,
        ))?;
    let shift_up = story.session().key_up(Key::Shift)?;
    let _settled = story
        .reaction(shift_up)
        .within(ReactionBudget::performance(Duration::from_millis(50)))
        .until(Condition::new(
            "released Shift to still the management mode",
            |state: &Observation| !state.jiggling,
        ))?;

    for thread in [TURN, INPUT, PERMISSION] {
        shift_click_card(story, thread)?;
        thread::sleep(Duration::from_millis(250));
        let frame = story.frame()?;
        demand(
            frame
                .state
                .cards
                .iter()
                .any(|card| card.thread == thread && card.open && !card.archived),
            format!("Shift retired active Codex session {thread}"),
        )?;
        demand(
            thread_archived(&fixture.index, thread) == Some(false),
            format!("Shift archived active Codex session {thread}"),
        )?;
    }
    Ok(())
}

fn shift_click_card(story: &mut Story<'_, '_, Observation>, thread: &str) -> Result<()> {
    story.session().focus()?;
    let target = story.anchor(CardTarget(Harness::Codex, thread))?;
    let (center_x, center_y) = target.center();
    let x = center_x.saturating_sub(100);
    let moved = story.session().move_to(x, center_y)?;
    let sought = thread.to_owned();
    let _hovered = story.reaction(moved).until(Condition::new(
        "management card to acquire the pointer",
        move |state: &Observation| hovered(state, Harness::Codex, &sought),
    ))?;
    let shift_down = story.session().key_down(Key::Shift)?;
    let _jiggling = story.reaction(shift_down).until(Condition::new(
        "management mode to acquire Shift",
        |state: &Observation| state.jiggling,
    ))?;
    let clicked = story.session().click(x, center_y, Button::Primary)?;
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
    park_pointer(
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
) -> Result<()> {
    story.session().focus()?;
    let target = story.anchor(CardTarget(Harness::Codex, thread))?;
    let (center_x, center_y) = target.center();
    let strike_x = center_x.saturating_sub(100);
    let moved = story.session().move_to(strike_x, center_y)?;
    let sought = thread.to_owned();
    let _hovered = story.reaction(moved).until(Condition::new(
        "resume card to acquire the pointer",
        move |state: &Observation| hovered(state, Harness::Codex, &sought),
    ))?;
    let selected = story.session().click(strike_x, center_y, Button::Primary)?;
    let _armed = story.reaction(selected).until(Condition::new(
        "card strike to enter flight",
        |state: &Observation| state.flight == Flight::Striking,
    ))?;
    app.wait_until(
        Duration::from_secs(10),
        "Codex session to be resumed in a fresh Alacritty",
        || Ok(proof.is_file()),
    )?;
    let _returned = story.session().key(Key::Function(7))?;
    app.wait_until(
        Duration::from_secs(8),
        "i3 to return to the fixed Wrangler workspace after resume",
        || Ok(wrangler_count(testbed)? == Some(1)),
    )?;
    let _landed = story.wait(Condition::new(
        "Codex session strike to leave flight",
        |state: &Observation| state.flight == Flight::Grounded,
    ))?;
    Ok(())
}

fn wait_card(
    story: &mut Story<'_, '_, Observation>,
    thread: &str,
    predicate: impl Fn(&contract::CardObservation) -> bool,
) -> Result<contract::CardObservation> {
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
    park_pointer(story, "pointer to park before the initial census")?;
    let frame = story.wait_stable(
        Duration::from_secs(30),
        Duration::from_millis(250),
        "eight live terminals and one remembered Codex session",
        |frame| {
            (frame.state.cards.len() == 9
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
    verify_census(&frame.state)?;
    verify_badge_dovetail(story)?;
    park_pointer(story, "pointer to leave the gallery before state mutation")?;
    verify_goal_truth(story, &fixture.goals)?;
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
            .within(ReactionBudget::performance(Duration::from_millis(50)))
            .until(Condition::new(
                format!("the entire tile hitbox to own its {region}"),
                move |state: &Observation| hovered(state, Harness::Codex, &thread),
            ))?;
    }

    park_pointer(story, "pointer to leave every tile before capture")?;
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

fn verify_census(state: &Observation) -> Result<()> {
    demand(
        state.fingerprint == UI_FINGERPRINT,
        "Codex Wrangler witness fingerprint drifted",
    )?;
    demand(!state.loading, "settled census still reports loading")?;
    let states = state
        .cards
        .iter()
        .map(|card| {
            (
                card.harness,
                card.thread.as_str(),
                card.work,
                card.name.as_deref(),
                card.workspace,
                card.open,
                card.archived,
            )
        })
        .collect::<BTreeSet<_>>();
    let expected = BTreeSet::from([
        (
            Harness::Codex,
            INPUT,
            Work::Input,
            Some("Awaiting verdict"),
            Some(7),
            true,
            false,
        ),
        (
            Harness::Codex,
            GOAL,
            Work::Goal,
            Some("Violet frontier"),
            Some(7),
            true,
            false,
        ),
        (Harness::Codex, TURN, Work::Turn, None, Some(7), true, false),
        (
            Harness::Codex,
            DONE,
            Work::Done,
            Some("Silent machine"),
            Some(7),
            true,
            false,
        ),
        (
            Harness::Codex,
            PERMISSION,
            Work::Input,
            None,
            Some(7),
            true,
            false,
        ),
        (
            Harness::Codex,
            ROTATE,
            Work::Done,
            Some("Old account"),
            Some(7),
            true,
            false,
        ),
        (
            Harness::Codex,
            DORMANT,
            Work::Done,
            Some("Buried engine"),
            Some(6),
            false,
            false,
        ),
        (
            Harness::ClaudeCode,
            CLAUDE,
            Work::Done,
            Some("Copper invader"),
            Some(7),
            true,
            false,
        ),
        (
            Harness::PrimeAgent,
            PRIME,
            Work::Turn,
            Some("Butterfly engine"),
            Some(7),
            true,
            false,
        ),
    ]);
    demand(states == expected, format!("wrong card census: {states:?}"))
}

fn verify_badge_dovetail(story: &mut Story<'_, '_, Observation>) -> Result<()> {
    for (harness, thread) in [
        (Harness::Codex, GOAL),
        (Harness::ClaudeCode, CLAUDE),
        (Harness::PrimeAgent, PRIME),
    ] {
        let logo = story.anchor(LogoTarget(harness, thread))?;
        let workspace = story.anchor(WorkspaceTarget(harness, thread))?;
        let [logo_left, logo_top, logo_right, logo_bottom] = logo.rect;
        let [
            workspace_left,
            workspace_top,
            workspace_right,
            workspace_bottom,
        ] = workspace.rect;
        demand(
            (logo_right - workspace_left).abs() <= 0.5
                && (logo_top - workspace_top).abs() <= 0.5
                && (logo_bottom - workspace_bottom).abs() <= 0.5,
            format!("{harness:?} logo does not dovetail immediately left of its workspace box"),
        )?;
        demand(
            logo_right - logo_left >= 27.0 && workspace_right > workspace_left,
            format!("{harness:?} badge geometry collapsed"),
        )?;
    }
    Ok(())
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

fn verify_hover_lock(story: &mut Story<'_, '_, Observation>, rollout: &Path) -> Result<()> {
    let target = story.anchor(CardTarget(Harness::Codex, INPUT))?;
    let receipt = story
        .session()
        .move_to(target.center().0, target.center().1)?;
    let thread = INPUT.to_owned();
    let _hovered = story
        .reaction(receipt)
        .within(ReactionBudget::performance(Duration::from_millis(50)))
        .until(Condition::new(
            "input tile to own the stationary pointer",
            move |state: &Observation| hovered(state, Harness::Codex, &thread),
        ))?;

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

    park_pointer(story, "pointer departure to release the census")?;
    wait_for_work(
        story,
        INPUT,
        Work::Turn,
        "released census to admit the resolved input",
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

fn park_pointer(story: &mut Story<'_, '_, Observation>, description: &'static str) -> Result<()> {
    let jolted = story.session().move_to(629, 31)?;
    let _jolt_frame = story
        .reaction(jolted)
        .within(ReactionBudget::functional(Duration::from_secs(2)))
        .next_frame()?;
    let receipt = story.session().move_to(630, 32)?;
    let _parked = story
        .reaction(receipt)
        .within(ReactionBudget::functional(Duration::from_secs(2)))
        .until(Condition::new(description, |state: &Observation| {
            state.hovered.is_none()
        }))?;
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
    input_rollout: PathBuf,
    proof: PathBuf,
    workspace_proof: PathBuf,
    launch_workspace_proof: PathBuf,
    tiled_proof: PathBuf,
    tiled_away_proof: PathBuf,
    tiled_home_proof: PathBuf,
    floating_proof: PathBuf,
    state: PathBuf,
    roster: PathBuf,
    index: PathBuf,
    rotate_resume: PathBuf,
    dormant_resume: PathBuf,
    archived_rollout: PathBuf,
}

impl Fixture {
    fn forge(testbed: &Testbed, binary: &Path) -> Result<Self> {
        let codex = testbed.create_private_dir("home/.codex/sessions/2026/08/03")?;
        let [goal, turn, done, input, permission, rotate, _dormant] = seed_rollouts(&codex)?;
        let db_path = testbed.private_path("home/.codex/state_5.sqlite")?;
        seed_index(&db_path)?;
        let goals = testbed.private_path("home/.codex/goals_1.sqlite")?;
        seed_goals(&goals)?;
        seed_names(testbed)?;
        let state = testbed.write_private("xdg/state/codex-wrangler/window-mode", b"tiled\n")?;
        let roster = seed_roster(testbed)?;
        let _rotate_work = testbed.create_private_dir("work/rotate")?;
        let _dormant_work = testbed.create_private_dir("work/dormant")?;
        let _archive = testbed.create_private_dir("home/.codex/archived_sessions")?;
        let (claude, prime) = seed_foreign_transcripts(testbed)?;

        let fake = testbed.write_private(
            "fake-codex.zsh",
            br#"rollout=$1
proof=$2
exec 9<"$rollout"
xprop -id "$WINDOWID" -f _NET_WM_DESKTOP 32c -set _NET_WM_DESKTOP 0
IFS= read -rk1 key
print -r -- "$key" > "$proof"
sleep 90
"#,
        )?;
        let fake_cli = forge_fake_cli(testbed)?;
        let proof = testbed.private_path("focus-proof")?;
        let rotate_resume = testbed.private_path(format!("resume-proof-{ROTATE}"))?;
        let dormant_resume = testbed.private_path(format!("resume-proof-{DORMANT}"))?;
        let archived_rollout = testbed.private_path(format!(
            "home/.codex/archived_sessions/{}",
            done.file_name().unwrap_or_default().to_string_lossy()
        ))?;
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
             bindsym F2 workspace number 8, exec --no-startup-id touch /test/workspace-proof\n\
             bindsym F3 kill\n\
             bindsym F4 workspace number 9, exec --no-startup-id touch /test/launch-workspace-proof\n\
             bindsym F5 floating disable, exec --no-startup-id touch /test/tiled-proof\n\
             bindsym F6 workspace number 8, exec --no-startup-id touch /test/tiled-away-proof\n\
             bindsym F7 workspace number 9, exec --no-startup-id touch /test/tiled-home-proof\n\
             bindsym F8 floating enable, exec --no-startup-id touch /test/floating-proof\n",
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
            ],
        )?;
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700))
            .map_err(io_verdict("make fixture wrapper executable"))?;
        fs::set_permissions(&fake_cli, fs::Permissions::from_mode(0o700))
            .map_err(io_verdict("make fake Codex CLI executable"))?;
        fs::set_permissions(&focus_probe, fs::Permissions::from_mode(0o700))
            .map_err(io_verdict("make focus probe executable"))?;
        fs::set_permissions(&desktop_recorder, fs::Permissions::from_mode(0o700))
            .map_err(io_verdict("make desktop recorder executable"))?;
        fs::set_permissions(&posture_probe, fs::Permissions::from_mode(0o700))
            .map_err(io_verdict("make posture probe executable"))?;
        demand(fake.is_file(), "fake Codex script was not created")?;
        demand(i3.is_file(), "private i3 config was not created")?;
        Ok(Self {
            wrapper,
            focus_probe,
            desktop_recorder,
            posture_probe,
            goals,
            input_rollout: input,
            proof,
            workspace_proof,
            launch_workspace_proof,
            tiled_proof,
            tiled_away_proof,
            tiled_home_proof,
            floating_proof,
            state,
            roster,
            index: db_path,
            rotate_resume,
            dormant_resume,
            archived_rollout,
        })
    }
}

fn forge_wrapper(testbed: &Testbed, binary: &Path, logs: [&Path; 8]) -> Result<PathBuf> {
    let names = logs.map(|path| path.file_name().unwrap_or_default().to_string_lossy());
    let wrapper = format!(
        "#!/bin/sh\n\
         export PATH=/test/bin:/usr/bin\n\
         i3 -c /test/i3.config &\n\
         for attempt in $(seq 1 100); do\n\
           i3-msg -t get_workspaces >/dev/null 2>&1 && break\n\
           sleep 0.02\n\
         done\n\
         i3-msg 'workspace number 7' >/dev/null\n\
         alacritty --title 'Goal Codex' -o 'window.position={{x=1500,y=0}}' -o 'window.dimensions={{columns=20,lines=5}}' -e bash -c \
           'exec -a codex zsh /test/fake-codex.zsh /test/home/.codex/sessions/2026/08/03/{} /test/focus-proof' &\n\
         alacritty --title 'Turn Codex' -o 'window.position={{x=1500,y=200}}' -o 'window.dimensions={{columns=20,lines=5}}' -e bash -c \
           'exec -a codex zsh /test/fake-codex.zsh /test/home/.codex/sessions/2026/08/03/{} /test/turn-proof' &\n\
         alacritty --title 'Done Codex' -o 'window.position={{x=1500,y=400}}' -o 'window.dimensions={{columns=20,lines=5}}' -e bash -c \
           'exec -a codex zsh /test/fake-codex.zsh /test/home/.codex/sessions/2026/08/03/{} /test/done-proof' &\n\
         alacritty --title 'Input Codex' -o 'window.position={{x=1500,y=600}}' -o 'window.dimensions={{columns=20,lines=5}}' -e bash -c \
           'exec -a codex zsh /test/fake-codex.zsh /test/home/.codex/sessions/2026/08/03/{} /test/input-proof' &\n\
         alacritty --title '[ ! ] Action Required | Permission Codex' -o 'window.position={{x=1500,y=800}}' -o 'window.dimensions={{columns=20,lines=5}}' -e bash -c \
           'exec -a codex zsh /test/fake-codex.zsh /test/home/.codex/sessions/2026/08/03/{} /test/permission-proof' &\n\
         alacritty --title 'Old Account Codex' -o 'window.position={{x=1500,y=1000}}' -o 'window.dimensions={{columns=20,lines=5}}' -e bash -c \
           'exec -a codex zsh /test/fake-codex.zsh /test/home/.codex/sessions/2026/08/03/{} /test/rotate-proof' &\n\
         alacritty --title 'Claude Code' -o 'window.position={{x=1500,y=800}}' -o 'window.dimensions={{columns=20,lines=5}}' -e bash -c \
           'exec -a claude zsh /test/fake-codex.zsh /test/home/.claude/projects/-work-claude/{} /test/claude-proof' &\n\
         alacritty --title 'Prime Agent' -o 'window.position={{x=1500,y=1000}}' -o 'window.dimensions={{columns=20,lines=5}}' -e bash -c \
           'exec -a prime-agent zsh /test/fake-codex.zsh /test/home/.prime/agent/sessions/{} /test/prime-proof' &\n\
         exec {}\n",
        names[0],
        names[1],
        names[2],
        names[3],
        names[4],
        names[5],
        names[6],
        names[7],
        binary.display(),
    );
    testbed.write_private("launch-wrangler", wrapper)
}

fn forge_fake_cli(testbed: &Testbed) -> Result<PathBuf> {
    let _bin = testbed.create_private_dir("bin")?;
    testbed.write_private(
        "bin/codex",
        format!(
            r#"#!/bin/zsh
db=/test/home/.codex/state_5.sqlite
if [[ $1 == app-server ]]; then
  while IFS= read -r request; do
    id=$(print -r -- "$request" | jq -r '.id // empty')
    method=$(print -r -- "$request" | jq -r '.method')
    [[ -z $id ]] && continue
    case $method in
      initialize)
        print -r -- '{{"id":'$id',"result":{{}}}}'
        ;;
      account/read)
        print -r -- '{{"id":'$id',"result":{{"account":{{"type":"chatgpt","email":"new@example.invalid","planType":"pro"}},"requiresOpenaiAuth":true}}}}'
        ;;
      account/rateLimits/read)
        print -r -- '{{"id":'$id',"result":{{"rateLimits":{{"limitId":"codex","primary":{{"usedPercent":1,"windowDurationMins":10080,"resetsAt":{NEW_RESET}}}}},"rateLimitsByLimitId":{{}}}}}}'
        ;;
      thread/archive)
        thread=$(print -r -- "$request" | jq -r '.params.threadId')
        rollout=$(sqlite3 "$db" "SELECT rollout_path FROM threads WHERE id = '$thread'")
        destination=/test/home/.codex/archived_sessions/${{rollout:t}}
        mv "$rollout" "$destination"
        sqlite3 "$db" "UPDATE threads SET archived = 1, rollout_path = '$destination' WHERE id = '$thread'"
        print -r -- '{{"id":'$id',"result":{{}}}}'
        ;;
      *)
        print -r -- '{{"id":'$id',"result":{{}}}}'
        ;;
    esac
  done
  exit 0
fi

operation=$1
thread=$2
if [[ $operation == unarchive ]]; then
  rollout=$(sqlite3 "$db" "SELECT rollout_path FROM threads WHERE id = '$thread'")
  destination=/test/home/.codex/sessions/2026/08/03/${{rollout:t}}
  mv "$rollout" "$destination"
  sqlite3 "$db" "UPDATE threads SET archived = 0, rollout_path = '$destination' WHERE id = '$thread'"
fi
workspace=$(i3-msg -t get_workspaces | jq -r '.[] | select(.focused).num')
print -r -- "$operation $workspace" > "/test/${{operation}}-proof-${{thread}}"
rollout=$(sqlite3 "$db" "SELECT rollout_path FROM threads WHERE id = '$thread'")
exec -a codex zsh -c 'exec 9<"$1"; sleep 90; :' wrangler-resume "$rollout"
"#,
        ),
    )
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
        let _inserted = db
            .execute(
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
           archived INTEGER NOT NULL DEFAULT 0
         );",
    )
    .map_err(verdict("declare fixture thread index"))?;
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
            "/work/turn",
            20,
            rollout_test_path(TURN, "turn"),
        ),
        (
            DONE,
            "Prompt-derived done title",
            None,
            "/work/done",
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
            UNSEEN,
            "Forbidden historical thread",
            Some("Never enumerate me"),
            "/test/work/dormant",
            7,
            rollout_test_path(UNSEEN, "unseen"),
        ),
    ] {
        let _inserted = db
            .execute(
                "INSERT INTO threads
                 (id, title, name, cwd, updated_at_ms, thread_source, source, agent_role,
                  rollout_path, archived)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'user', 'cli', NULL, ?6, 0)",
                params![row.0, row.1, row.2, row.3, row.4, row.5],
            )
            .map_err(verdict("seed fixture thread"))?;
    }
    Ok(())
}

fn seed_roster(testbed: &Testbed) -> Result<PathBuf> {
    let state = serde_json::json!({
        "version": 1,
        "sessions": {
            DORMANT: {
                "name": "Buried engine",
                "cwd": "/test/work/dormant",
                "preview": "Waiting below.",
                "updated_at_ms": 8,
                "workspace": 6,
                "retention": "active",
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

fn seed_rollouts(directory: &Path) -> Result<[PathBuf; 7]> {
    let goal = rollout(directory, GOAL, "goal");
    let turn = rollout(directory, TURN, "turn");
    let done = rollout(directory, DONE, "done");
    let input = rollout(directory, INPUT, "input");
    let permission = rollout(directory, PERMISSION, "permission");
    let rotate = rollout(directory, ROTATE, "rotate");
    let dormant = rollout(directory, DORMANT, "dormant");
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
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"Cut the ordinary task through a deliberately immense preview which must remain imprisoned inside the card. It keeps going through several clauses, several sentences, and enough visual matter to expose any label whose clip rectangle is merely aspirational rather than real. None of this text may trespass into the next row, however narrow the window becomes.\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_goal_updated\",\"goal\":{\"status\":\"active\"}}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n"
        ),
    )
    .map_err(io_verdict("write running rollout"))?;
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
    Ok([goal, turn, done, input, permission, rotate, dormant])
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
