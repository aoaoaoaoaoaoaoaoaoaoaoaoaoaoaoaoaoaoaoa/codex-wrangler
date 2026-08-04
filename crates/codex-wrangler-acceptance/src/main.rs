use std::{
    collections::BTreeSet,
    env, fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    time::Duration,
};

use egui_tester::{
    AppCommand, Application, Condition, Error as TesterError, Key, ReactionBudget, Result, Story,
    Testbed, TestbedBuilder, WindowQuery, demand,
};
use rusqlite::{Connection, params};

#[path = "../../codex-wrangler/src/contract.rs"]
mod contract;
use contract::{CardTarget, Observation, UI_FINGERPRINT, Work};

const GOAL: &str = "10000000-0000-7000-8000-000000000001";
const TURN: &str = "20000000-0000-7000-8000-000000000002";
const DONE: &str = "30000000-0000-7000-8000-000000000003";

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
    let mut story: Story<'_, '_, Observation> = Story::bind(
        testbed,
        &app,
        WindowQuery::title_exact("Codex Wrangler"),
        ReactionBudget::functional(Duration::from_secs(10)),
    )?;
    verify_gallery(testbed, &mut story)?;

    let target = story.anchor(CardTarget(GOAL))?;
    let _clicked = story.session().click(
        target.center().0,
        target.center().1,
        egui_tester::Button::Primary,
    )?;
    app.wait_until(
        Duration::from_secs(8),
        "Wrangler to remain visible after choosing a Codex terminal",
        || Ok(wrangler_count(testbed)?.is_some_and(|count| count > 0)),
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
    let wrangler = x11.wait_window_query(
        app,
        WindowQuery::title_exact("Codex Wrangler"),
        Duration::from_secs(8),
    )?;
    x11.focus(&wrangler)?;
    let _escape = story.session().key(Key::Escape)?;
    app.wait_until(
        Duration::from_secs(8),
        "Escape to conceal the gallery",
        || Ok(wrangler_count(testbed)? == Some(0)),
    )?;
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

fn verify_gallery(testbed: &Testbed, story: &mut Story<'_, '_, Observation>) -> Result<()> {
    let frame = story.wait_stable(
        Duration::from_secs(30),
        Duration::from_millis(250),
        "three manual Codex terminals",
        |frame| {
            (frame.state.cards.len() == 3
                && frame
                    .state
                    .cards
                    .iter()
                    .all(|card| card.workspace == Some(7)))
            .then(|| frame.state.cards.clone())
        },
    )?;
    demand(
        frame.state.fingerprint == UI_FINGERPRINT,
        "Codex Wrangler witness fingerprint drifted",
    )?;
    demand(!frame.state.loading, "settled census still reports loading")?;
    let states = frame
        .state
        .cards
        .iter()
        .map(|card| {
            (
                card.thread.as_str(),
                card.work,
                card.name.as_deref(),
                card.workspace,
            )
        })
        .collect::<BTreeSet<_>>();
    demand(
        states
            == BTreeSet::from([
                (GOAL, Work::Goal, Some("Violet frontier"), Some(7)),
                (TURN, Work::Turn, None, Some(7)),
                (DONE, Work::Done, Some("Silent machine"), Some(7)),
            ]),
        format!("wrong card census: {states:?}"),
    )?;

    let turn = story.anchor(CardTarget(TURN))?;
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
                move |state: &Observation| state.hovered.as_deref() == Some(&thread),
            ))?;
    }

    let capture = testbed.private_path("captures/wrangler.png")?;
    story.capture()?.save_png(&capture)?;
    testbed.retain_on_failure("captures/wrangler.png")?;
    if let Some(destination) = env::var_os("CODEX_WRANGLER_ACCEPTANCE_CAPTURE") {
        fs::copy(&capture, destination).map_err(io_verdict("export acceptance capture"))?;
    }
    Ok(())
}

struct Fixture {
    wrapper: PathBuf,
    proof: PathBuf,
    workspace_proof: PathBuf,
    launch_workspace_proof: PathBuf,
}

impl Fixture {
    fn forge(testbed: &Testbed, binary: &Path) -> Result<Self> {
        let codex = testbed.create_private_dir("home/.codex/sessions/2026/08/03")?;
        let db_path = testbed.private_path("home/.codex/state_5.sqlite")?;
        seed_index(&db_path)?;
        seed_names(testbed)?;
        let [goal, turn, done] = seed_rollouts(&codex)?;

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
        let i3 = testbed.write_private(
            "i3.config",
            "font pango:monospace 8\n\
             focus_follows_mouse no\n\
             bindsym F2 workspace number 8, exec --no-startup-id touch /test/workspace-proof\n\
             bindsym F3 kill\n\
             bindsym F4 workspace number 9, exec --no-startup-id touch /test/launch-workspace-proof\n",
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
             exec {binary}\n",
            goal.file_name().unwrap_or_default().to_string_lossy(),
            turn.file_name().unwrap_or_default().to_string_lossy(),
            done.file_name().unwrap_or_default().to_string_lossy(),
        );
        let wrapper = testbed.write_private("launch-wrangler", wrapper_text)?;
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700))
            .map_err(io_verdict("make fixture wrapper executable"))?;
        demand(fake.is_file(), "fake Codex script was not created")?;
        demand(i3.is_file(), "private i3 config was not created")?;
        Ok(Self {
            wrapper,
            proof,
            workspace_proof,
            launch_workspace_proof,
        })
    }
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
            "{{\"id\":\"{done}\",\"thread_name\":\"Silent machine\"}}\n"
        ),
        goal = GOAL,
        done = DONE
    );
    let index = testbed.write_private("home/.codex/session_index.jsonl", names)?;
    demand(index.is_file(), "session-name index was not created")
}

fn seed_rollouts(directory: &Path) -> Result<[PathBuf; 3]> {
    let goal = rollout(directory, GOAL, "goal");
    let turn = rollout(directory, TURN, "turn");
    let done = rollout(directory, DONE, "done");
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
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_goal_updated\",\"goal\":{\"status\":\"paused\"}}}\n",
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
    Ok([goal, turn, done])
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
