use std::{
    collections::BTreeSet,
    fmt::Write as _,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    time::Duration,
};

use egui_tester::{
    AppCommand, Error as TesterError, Graphics, ReactionBudget, Result, Story, Testbed,
    WindowQuery, demand,
};
use rusqlite::{Connection, params};
use serde::Deserialize;
use serde_json::Value;

use super::{
    APPLICATION_APPEARANCE_CEILING, FIXTURE_POLL_INTERVAL_MILLIS, I3_READINESS_POLLS,
    TERMINAL_READINESS_POLLS, arm_executable, forge_fake_cli, forge_replaceable_alacritty,
    io_verdict, vacate_gallery, wait_named_window,
};
use codex_wrangler_contract::{Observation, Work};

const CORPUS: &str = include_str!("../../../fixtures/showcase.json");

#[derive(Deserialize)]
struct Corpus {
    sessions: Vec<Session>,
}

#[derive(Deserialize)]
struct Session {
    id: String,
    slug: String,
    name: String,
    cwd: String,
    updated_at_ms: i64,
    workspace: u32,
    state: ShowcaseState,
    rollout: Vec<Value>,
}

#[derive(Clone, Copy, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
enum ShowcaseState {
    Input,
    Goal,
    Working,
    Done,
    Closed,
}

impl ShowcaseState {
    const fn work(self) -> Work {
        match self {
            Self::Input => Work::Input,
            Self::Goal => Work::Goal,
            Self::Working => Work::Turn,
            Self::Done => Work::Done,
            Self::Closed => Work::Closed,
        }
    }

    const fn live(self) -> bool {
        !matches!(self, Self::Closed)
    }

    const fn terminal_title(self) -> &'static str {
        match self {
            Self::Input => "Input Codex",
            Self::Goal => "Goal Codex",
            Self::Working => "Working Codex",
            Self::Done => "Done Codex",
            Self::Closed => "Closed Codex",
        }
    }
}

struct Fixture {
    wrapper: PathBuf,
    expected: BTreeSet<(String, Work, String, u32)>,
}

pub fn story(testbed: &Testbed, binary: &Path, destination: &Path) -> Result<()> {
    let fixture = Fixture::forge(testbed, binary)?;
    let app = testbed.launch(
        AppCommand::new(&fixture.wrapper)
            .borrow_read_only(binary)
            .graphics(Graphics::Host)
            .private_env("CODEX_HOME", "home/.codex")
            .witness("probes/showcase")
            .runtime(Duration::from_secs(90)),
    )?;
    let _window = wait_named_window(
        testbed,
        &app,
        "Codex Wrangler",
        APPLICATION_APPEARANCE_CEILING,
    )?;
    let mut story: Story<'_, '_, Observation> = Story::bind(
        testbed,
        &app,
        WindowQuery::title_exact("Codex Wrangler"),
        ReactionBudget::functional(Duration::from_secs(10)),
    )?;
    vacate_gallery(&mut story, "pointer to vacate the synthetic showcase")?;
    let _settled = story.wait_stable(
        Duration::from_secs(30),
        Duration::from_millis(500),
        "the five-state synthetic showcase snapshot",
        |frame| snapshot(&frame.state).eq(&fixture.expected).then_some(()),
    )?;
    vacate_gallery(&mut story, "pointer to vacate every showcase card")?;
    let _calm = story.wait_stable(
        Duration::from_secs(4),
        Duration::from_millis(500),
        "showcase water to settle before capture",
        |frame| frame.state.hovered.is_none().then_some(()),
    )?;
    let capture = testbed.private_path("captures/showcase.png")?;
    story.capture()?.save_png(&capture)?;
    testbed.retain_on_failure("captures/showcase.png")?;
    fs::copy(&capture, destination).map_err(io_verdict("export showcase capture"))?;
    Ok(())
}

impl Fixture {
    fn forge(testbed: &Testbed, binary: &Path) -> Result<Self> {
        let corpus = decode_corpus()?;
        let directory = testbed.create_private_dir("home/.codex/sessions/2026/08/14")?;
        for session in &corpus.sessions {
            let path = rollout(&directory, session);
            write_events(&path, &session.rollout)?;
        }
        seed_index(
            &testbed.private_path("home/.codex/state_5.sqlite")?,
            &corpus,
        )?;
        seed_goals(
            &testbed.private_path("home/.codex/goals_1.sqlite")?,
            &corpus,
        )?;
        seed_roster(testbed, &corpus)?;
        let _mode = testbed.write_private("xdg/state/codex-wrangler/window-mode", b"floating\n")?;
        let fake_cli = forge_fake_cli(testbed)?;
        let terminal = forge_replaceable_alacritty(testbed)?;
        let session = forge_session(testbed)?;
        let i3 = forge_i3(testbed, &corpus)?;
        let wrapper = forge_wrapper(testbed, binary, &corpus)?;
        arm_executable(&fake_cli, "make showcase Codex CLI executable")?;
        arm_executable(&session, "make showcase session executable")?;
        arm_executable(&wrapper, "make showcase wrapper executable")?;
        demand(
            terminal.is_file(),
            "showcase terminal fixture was not created",
        )?;
        demand(i3.is_file(), "showcase i3 config was not created")?;
        let expected = corpus
            .sessions
            .iter()
            .map(|session| {
                (
                    session.id.clone(),
                    session.state.work(),
                    session.name.clone(),
                    session.workspace,
                )
            })
            .collect();
        Ok(Self { wrapper, expected })
    }
}

fn decode_corpus() -> Result<Corpus> {
    let corpus = serde_json::from_str::<Corpus>(CORPUS).map_err(|error| TesterError::Verdict {
        detail: format!("decode synthetic showcase corpus: {error}"),
    })?;
    let states = corpus
        .sessions
        .iter()
        .map(|session| session.state)
        .collect::<BTreeSet<_>>();
    let all_states = BTreeSet::from([
        ShowcaseState::Input,
        ShowcaseState::Goal,
        ShowcaseState::Working,
        ShowcaseState::Done,
        ShowcaseState::Closed,
    ]);
    demand(
        states == all_states,
        "showcase corpus must own exactly five lifecycle states",
    )?;
    demand(
        corpus.sessions.len() == states.len(),
        "showcase corpus must own one session per lifecycle state",
    )?;
    demand(
        corpus
            .sessions
            .iter()
            .all(|session| !session.rollout.is_empty()),
        "every showcase session must own a synthetic rollout",
    )?;
    Ok(corpus)
}

fn seed_index(path: &Path, corpus: &Corpus) -> Result<()> {
    let db = Connection::open(path).map_err(verdict("create showcase thread index"))?;
    db.execute_batch(
        "CREATE TABLE threads (
           id TEXT PRIMARY KEY, title TEXT NOT NULL, name TEXT, cwd TEXT NOT NULL,
           updated_at_ms INTEGER NOT NULL, thread_source TEXT, source TEXT NOT NULL,
           agent_role TEXT, rollout_path TEXT NOT NULL,
           archived INTEGER NOT NULL DEFAULT 0
         );",
    )
    .map_err(verdict("declare showcase thread index"))?;
    for session in &corpus.sessions {
        db.execute(
            "INSERT INTO threads
             (id, title, name, cwd, updated_at_ms, thread_source, source, agent_role,
              rollout_path, archived)
             VALUES (?1, ?2, ?2, ?3, ?4, 'user', 'cli', NULL, ?5, ?6)",
            params![
                session.id,
                session.name,
                session.cwd,
                session.updated_at_ms,
                rollout_test_path(session),
                i32::from(session.state == ShowcaseState::Closed),
            ],
        )
        .map_err(verdict("seed showcase thread"))?;
    }
    Ok(())
}

fn seed_goals(path: &Path, corpus: &Corpus) -> Result<()> {
    let db = Connection::open(path).map_err(verdict("create showcase goal ledger"))?;
    db.execute_batch(
        "CREATE TABLE thread_goals (
           thread_id TEXT PRIMARY KEY NOT NULL,
           status TEXT NOT NULL
         );",
    )
    .map_err(verdict("declare showcase goal ledger"))?;
    for session in &corpus.sessions {
        db.execute(
            "INSERT INTO thread_goals (thread_id, status) VALUES (?1, ?2)",
            params![
                session.id,
                if session.state == ShowcaseState::Goal {
                    "active"
                } else {
                    "complete"
                },
            ],
        )
        .map_err(verdict("seed showcase goal"))?;
    }
    Ok(())
}

fn seed_roster(testbed: &Testbed, corpus: &Corpus) -> Result<PathBuf> {
    let closed = corpus
        .sessions
        .iter()
        .find(|session| session.state == ShowcaseState::Closed)
        .ok_or_else(|| TesterError::Verdict {
            detail: "showcase corpus omitted its closed session".to_owned(),
        })?;
    let preview = event_preview(closed.rollout.last()).ok_or_else(|| TesterError::Verdict {
        detail: "showcase closed session omitted its terminal preview".to_owned(),
    })?;
    let state = serde_json::json!({
        "version": 2,
        "sessions": {
            &closed.id: {
                "name": &closed.name,
                "cwd": &closed.cwd,
                "preview": preview,
                "updated_at_ms": closed.updated_at_ms,
                "workspace": closed.workspace
            }
        }
    });
    let bytes = serde_json::to_vec(&state).map_err(|error| TesterError::Verdict {
        detail: format!("encode showcase roster: {error}"),
    })?;
    testbed.write_private("xdg/state/codex-wrangler/known-sessions.json", bytes)
}

fn forge_i3(testbed: &Testbed, corpus: &Corpus) -> Result<PathBuf> {
    let mut config = String::from(
        "font pango:monospace 8\n\
         focus_follows_mouse no\n\
         workspace_layout tabbed\n\
         for_window [title=\"^Codex Wrangler$\"] floating enable, move position 0 0\n",
    );
    for session in corpus
        .sessions
        .iter()
        .filter(|session| session.state.live())
    {
        writeln!(
            config,
            "assign [title=\"^{}$\"] workspace number {}",
            session.state.terminal_title(),
            session.workspace
        )
        .expect("writing to a String is infallible");
    }
    config.push_str(
        "bar {\n\
           mode dock\n\
           position bottom\n\
           tray_output screen\n\
         }\n",
    );
    testbed.write_private("i3.config", config)
}

fn forge_session(testbed: &Testbed) -> Result<PathBuf> {
    testbed.write_private(
        "showcase-session.bash",
        b"#!/bin/bash\nset -eu\nexec 9>>\"$1\"\nexec -a codex sleep 90\n",
    )
}

fn forge_wrapper(testbed: &Testbed, binary: &Path, corpus: &Corpus) -> Result<PathBuf> {
    let mut launches = String::new();
    let live = corpus
        .sessions
        .iter()
        .filter(|session| session.state.live())
        .count();
    for session in corpus
        .sessions
        .iter()
        .filter(|session| session.state.live())
    {
        write!(
            launches,
            r#""$terminal" --class NeutralTerminal --title '{}' \
  -e /test/showcase-session.bash \
  /test/home/.codex/sessions/2026/08/14/{} &
terminal_pids="$terminal_pids $!"
"#,
            session.state.terminal_title(),
            rollout_name(session),
        )
        .expect("writing to a String is infallible");
    }
    let wrapper = format!(
        "#!/bin/sh\n\
         set -eu\n\
         export PATH=/test/bin:/usr/bin\n\
         terminal=/test/bin/alacritty-0.16.1-x11-ime\n\
         terminal_pids=\n\
         i3 -c /test/i3.config &\n\
         for attempt in $(seq 1 {I3_READINESS_POLLS}); do\n\
           i3-msg -t get_workspaces >/dev/null 2>&1 && break\n\
           sleep 0.{FIXTURE_POLL_INTERVAL_MILLIS:03}\n\
         done\n\
         {launches}\
         fixture_windows=0\n\
         ready=0\n\
         for attempt in $(seq 1 {TERMINAL_READINESS_POLLS}); do\n\
           fixture_windows=$(i3-msg -t get_tree 2>/dev/null | jq '[.. | .window? | select(. != null)] | length')\n\
           ready=1\n\
           [ \"$fixture_windows\" -ge {live} ] || ready=0\n\
           for pid in $terminal_pids; do\n\
             case $(readlink \"/proc/$pid/exe\" 2>/dev/null) in\n\
               */alacritty-0.16.1-x11-ime) ;;\n\
               *) ready=0; break ;;\n\
             esac\n\
           done\n\
           [ \"$ready\" = 1 ] && break\n\
           sleep 0.{FIXTURE_POLL_INTERVAL_MILLIS:03}\n\
         done\n\
         [ \"$ready\" = 1 ] || exit 70\n\
         i3-msg 'workspace number 7' >/dev/null\n\
         exec {}\n",
        binary.display(),
    );
    testbed.write_private("launch-showcase", wrapper)
}

fn write_events(path: &Path, events: &[Value]) -> Result<()> {
    let mut bytes = Vec::new();
    for event in events {
        writeln!(&mut bytes, "{event}").map_err(io_verdict("encode showcase rollout"))?;
    }
    fs::write(path, bytes).map_err(io_verdict("write showcase rollout"))
}

fn event_preview(event: Option<&Value>) -> Option<&str> {
    event?.get("payload")?.get("last_agent_message")?.as_str()
}

fn rollout(directory: &Path, session: &Session) -> PathBuf {
    directory.join(rollout_name(session))
}

fn rollout_name(session: &Session) -> String {
    format!(
        "rollout-2026-08-14T00-00-00-{}-{}.jsonl",
        session.slug, session.id
    )
}

fn rollout_test_path(session: &Session) -> String {
    format!(
        "/test/home/.codex/sessions/2026/08/14/{}",
        rollout_name(session)
    )
}

fn snapshot(state: &Observation) -> BTreeSet<(String, Work, String, u32)> {
    state
        .cards
        .iter()
        .filter_map(|card| {
            Some((
                card.thread.clone(),
                card.work,
                card.name.clone()?,
                card.workspace?,
            ))
        })
        .collect()
}

fn verdict(operation: &'static str) -> impl FnOnce(rusqlite::Error) -> TesterError {
    move |error| TesterError::Verdict {
        detail: format!("{operation}: {error}"),
    }
}
