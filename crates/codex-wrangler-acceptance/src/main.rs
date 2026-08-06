use std::{
    collections::BTreeSet,
    env, fs,
    io::Write as _,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    time::Duration,
};

use egui_tester::{
    AppCommand, Application, Condition, Error as TesterError, Key, ReactionBudget, Result, Story,
    Testbed, TestbedBuilder, Window, WindowQuery, demand,
};
use rusqlite::{Connection, params};

#[path = "../../codex-wrangler/src/contract.rs"]
mod contract;
use contract::{
    CardKey, CardTarget, Harness, LogoTarget, Observation, UI_FINGERPRINT, Work, WorkspaceTarget,
};

const GOAL: &str = "10000000-0000-7000-8000-000000000001";
const TURN: &str = "20000000-0000-7000-8000-000000000002";
const DONE: &str = "30000000-0000-7000-8000-000000000003";
const INPUT: &str = "40000000-0000-7000-8000-000000000004";
const CLAUDE: &str = "50000000-0000-7000-8000-000000000005";
const PRIME: &str = "60000000-0000-7000-8000-000000000006";

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
    let wrangler = testbed.x11()?.wait_window_query(
        &app,
        WindowQuery::title_exact("Codex Wrangler"),
        Duration::from_secs(8),
    )?;
    verify_switcher_present(testbed, &fixture, wrangler.id(), false)?;
    let mut story: Story<'_, '_, Observation> = Story::bind(
        testbed,
        &app,
        WindowQuery::title_exact("Codex Wrangler"),
        ReactionBudget::functional(Duration::from_secs(10)),
    )?;
    verify_gallery(testbed, &mut story, &fixture)?;

    let target = story.anchor(CardTarget(Harness::Codex, GOAL))?;
    let _clicked = story.session().click(
        target.center().0,
        target.center().1,
        egui_tester::Button::Primary,
    )?;
    app.wait_until(
        Duration::from_secs(8),
        "Wrangler to conceal itself after choosing a Codex terminal",
        || Ok(wrangler_count(testbed)? == Some(0)),
    )?;
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

fn verify_residency(
    testbed: &Testbed,
    binary: &Path,
    app: &Application<'_>,
    story: &mut Story<'_, '_, Observation>,
    fixture: &Fixture,
) -> Result<()> {
    let x11 = testbed.x11()?;
    let wrangler_id = story.session().window().id();
    let tray = x11.wait_window_query(
        app,
        WindowQuery::title_exact("Codex Wrangler tray"),
        Duration::from_secs(8),
    )?;
    let _switched = story.session().key(Key::Function(2))?;
    app.wait_until(
        Duration::from_secs(8),
        "i3 to enter a different workspace",
        || Ok(fixture.workspace_proof.is_file()),
    )?;
    let _clicked = x11.click(&tray, 12, 12, egui_tester::Button::Primary)?;
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

    let _menu_opened = x11.click(&tray, 12, 12, egui_tester::Button::Secondary)?;
    let menu = x11.wait_window_query(
        app,
        WindowQuery::title_exact("Codex Wrangler tray menu"),
        Duration::from_secs(8),
    )?;
    let _quit = x11.click(&menu, 70, 15, egui_tester::Button::Primary)?;
    let exit = app.wait(Duration::from_secs(8))?;
    demand(
        exit.success(),
        format!("tray quit did not end Wrangler cleanly: {exit:?}"),
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
    let _selected = story.session().click(
        target.center().0,
        target.center().1,
        egui_tester::Button::Primary,
    )?;
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
    let _summoned = testbed
        .x11()?
        .click(tray, 12, 12, egui_tester::Button::Primary)?;
    verify_switcher_present(testbed, fixture, wrangler, true)
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

fn verify_gallery(
    testbed: &Testbed,
    story: &mut Story<'_, '_, Observation>,
    fixture: &Fixture,
) -> Result<()> {
    park_pointer(story, "pointer to park before the initial census")?;
    let frame = story.wait_stable(
        Duration::from_secs(30),
        Duration::from_millis(250),
        "six manual terminals across three harnesses",
        |frame| {
            (frame.state.cards.len() == 6
                && frame
                    .state
                    .cards
                    .iter()
                    .all(|card| card.workspace == Some(7)))
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
        ),
        (
            Harness::Codex,
            GOAL,
            Work::Goal,
            Some("Violet frontier"),
            Some(7),
        ),
        (Harness::Codex, TURN, Work::Turn, None, Some(7)),
        (
            Harness::Codex,
            DONE,
            Work::Done,
            Some("Silent machine"),
            Some(7),
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
    goals: PathBuf,
    input_rollout: PathBuf,
    proof: PathBuf,
    workspace_proof: PathBuf,
    launch_workspace_proof: PathBuf,
    tiled_proof: PathBuf,
    tiled_away_proof: PathBuf,
    tiled_home_proof: PathBuf,
}

impl Fixture {
    fn forge(testbed: &Testbed, binary: &Path) -> Result<Self> {
        let codex = testbed.create_private_dir("home/.codex/sessions/2026/08/03")?;
        let db_path = testbed.private_path("home/.codex/state_5.sqlite")?;
        seed_index(&db_path)?;
        let goals = testbed.private_path("home/.codex/goals_1.sqlite")?;
        seed_goals(&goals)?;
        seed_names(testbed)?;
        let [goal, turn, done, input] = seed_rollouts(&codex)?;
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
        let proof = testbed.private_path("focus-proof")?;
        let workspace_proof = testbed.private_path("workspace-proof")?;
        let launch_workspace_proof = testbed.private_path("launch-workspace-proof")?;
        let tiled_proof = testbed.private_path("tiled-proof")?;
        let tiled_away_proof = testbed.private_path("tiled-away-proof")?;
        let tiled_home_proof = testbed.private_path("tiled-home-proof")?;
        let (focus_probe, desktop_recorder) = forge_desktop_probes(testbed)?;
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
             bindsym F7 workspace number 9, exec --no-startup-id touch /test/tiled-home-proof\n",
        )?;
        let binary = binary.display();
        let wrapper_text = format!(
            "#!/bin/sh\n\
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
             alacritty --title 'Claude Code' -o 'window.position={{x=1500,y=800}}' -o 'window.dimensions={{columns=20,lines=5}}' -e bash -c \
               'exec -a claude zsh /test/fake-codex.zsh /test/home/.claude/projects/-work-claude/{} /test/claude-proof' &\n\
             alacritty --title 'Prime Agent' -o 'window.position={{x=1500,y=1000}}' -o 'window.dimensions={{columns=20,lines=5}}' -e bash -c \
               'exec -a prime-agent zsh /test/fake-codex.zsh /test/home/.prime/agent/sessions/{} /test/prime-proof' &\n\
             exec {binary}\n",
            goal.file_name().unwrap_or_default().to_string_lossy(),
            turn.file_name().unwrap_or_default().to_string_lossy(),
            done.file_name().unwrap_or_default().to_string_lossy(),
            input.file_name().unwrap_or_default().to_string_lossy(),
            claude.file_name().unwrap_or_default().to_string_lossy(),
            prime.file_name().unwrap_or_default().to_string_lossy(),
        );
        let wrapper = testbed.write_private("launch-wrangler", wrapper_text)?;
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700))
            .map_err(io_verdict("make fixture wrapper executable"))?;
        fs::set_permissions(&focus_probe, fs::Permissions::from_mode(0o700))
            .map_err(io_verdict("make focus probe executable"))?;
        fs::set_permissions(&desktop_recorder, fs::Permissions::from_mode(0o700))
            .map_err(io_verdict("make desktop recorder executable"))?;
        demand(fake.is_file(), "fake Codex script was not created")?;
        demand(i3.is_file(), "private i3 config was not created")?;
        Ok(Self {
            wrapper,
            focus_probe,
            desktop_recorder,
            goals,
            input_rollout: input,
            proof,
            workspace_proof,
            launch_workspace_proof,
            tiled_proof,
            tiled_away_proof,
            tiled_home_proof,
        })
    }
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

fn forge_desktop_probes(testbed: &Testbed) -> Result<(PathBuf, PathBuf)> {
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
    Ok((focus, recorder))
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
           agent_role TEXT
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
        ),
        (
            TURN,
            "This first prompt must never become the displayed name",
            None,
            "/work/turn",
            20,
        ),
        (DONE, "Prompt-derived done title", None, "/work/done", 10),
        (INPUT, "Prompt-derived input title", None, "/work/input", 40),
    ] {
        let _inserted = db
            .execute(
                "INSERT INTO threads
                 (id, title, name, cwd, updated_at_ms, thread_source, source, agent_role)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'user', 'cli', NULL)",
                params![row.0, row.1, row.2, row.3, row.4],
            )
            .map_err(verdict("seed fixture thread"))?;
    }
    Ok(())
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

fn seed_rollouts(directory: &Path) -> Result<[PathBuf; 4]> {
    let goal = rollout(directory, GOAL, "goal");
    let turn = rollout(directory, TURN, "turn");
    let done = rollout(directory, DONE, "done");
    let input = rollout(directory, INPUT, "input");
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
    Ok([goal, turn, done, input])
}

fn rollout(directory: &Path, id: &str, stamp: &str) -> PathBuf {
    directory.join(format!("rollout-2026-08-03T00-00-00-{stamp}-{id}.jsonl"))
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
