use std::{
    collections::HashMap,
    fs::{self, File},
    io::{BufRead as _, BufReader, Read as _, Seek as _, SeekFrom},
    path::{Path, PathBuf},
};

use crate::contract::Work;
use memchr::memmem;
use serde_json::Value;

const BLOCK: usize = 1 << 20;

#[derive(Clone, Debug, Default)]
struct Pulse {
    goal: bool,
    running: bool,
    preview: String,
}

impl Pulse {
    fn work(&self) -> Work {
        if self.goal && self.running {
            Work::Goal
        } else if self.running {
            Work::Turn
        } else {
            Work::Done
        }
    }

    fn absorb(&mut self, line: &[u8]) {
        if !interesting(line) {
            return;
        }
        let Ok(event) = serde_json::from_slice::<Value>(line) else {
            return;
        };
        if event.get("type").and_then(Value::as_str) != Some("event_msg") {
            return;
        }
        let payload = &event["payload"];
        match payload.get("type").and_then(Value::as_str) {
            Some("task_started" | "turn_started") => self.running = true,
            Some("task_complete" | "turn_complete" | "turn_aborted") => {
                self.running = false;
                if let Some(message) = payload.get("last_agent_message").and_then(Value::as_str) {
                    assign_preview(&mut self.preview, message);
                }
            }
            Some("thread_goal_updated") => {
                self.goal = payload["goal"]["status"].as_str() == Some("active");
            }
            Some("user_message" | "agent_message") => {
                if let Some(message) = payload.get("message").and_then(Value::as_str) {
                    assign_preview(&mut self.preview, message);
                }
            }
            _ => {}
        }
    }
}

fn assign_preview(slot: &mut String, message: &str) {
    if !message.trim().is_empty() {
        message.trim().clone_into(slot);
    }
}

fn interesting(line: &[u8]) -> bool {
    [
        b"\"type\":\"task_".as_slice(),
        b"\"type\":\"turn_".as_slice(),
        b"\"type\":\"thread_goal_updated\"".as_slice(),
        b"\"type\":\"user_message\"".as_slice(),
        b"\"type\":\"agent_message\"".as_slice(),
    ]
    .iter()
    .any(|needle| memmem::find(line, needle).is_some())
}

#[derive(Clone, Debug)]
struct Memo {
    length: u64,
    pulse: Pulse,
}

#[derive(Default)]
pub struct Rollouts {
    memo: HashMap<PathBuf, Memo>,
}

#[derive(Clone, Debug)]
pub struct RolloutSummary {
    pub preview: String,
    pub work: Work,
}

impl Rollouts {
    pub fn read(&mut self, path: &Path) -> std::io::Result<RolloutSummary> {
        let length = fs::metadata(path)?.len();
        let pulse = match self.memo.get(path) {
            Some(memo) if memo.length == length => memo.pulse.clone(),
            Some(memo) if memo.length < length => {
                let mut pulse = memo.pulse.clone();
                absorb_suffix(path, memo.length, &mut pulse)?;
                pulse
            }
            _ => scan_reverse(path, length)?,
        };
        let summary = RolloutSummary {
            preview: pulse.preview.clone(),
            work: pulse.work(),
        };
        let _prior = self.memo.insert(path.to_owned(), Memo { length, pulse });
        Ok(summary)
    }
}

fn absorb_suffix(path: &Path, offset: u64, pulse: &mut Pulse) -> std::io::Result<()> {
    let mut file = File::open(path)?;
    let _position = file.seek(SeekFrom::Start(offset))?;
    for line in BufReader::new(file).split(b'\n') {
        pulse.absorb(&line?);
    }
    Ok(())
}

fn scan_reverse(path: &Path, length: u64) -> std::io::Result<Pulse> {
    let mut file = File::open(path)?;
    let mut cursor = length;
    let mut suffix = Vec::new();
    let mut newest = Pulse::default();
    let mut found_work = false;
    let mut found_goal = false;
    let mut found_preview = false;

    while cursor > 0 && !(found_work && found_goal && found_preview) {
        let start = cursor.saturating_sub(BLOCK as u64);
        let span = usize::try_from(cursor - start).unwrap_or(BLOCK);
        let mut bytes = vec![0; span];
        let _position = file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut bytes)?;
        bytes.extend_from_slice(&suffix);

        let first_break = memchr::memchr(b'\n', &bytes);
        let complete_from = if start == 0 {
            0
        } else {
            first_break.map_or(bytes.len(), |index| index + 1)
        };
        for line in bytes[complete_from..].split(|byte| *byte == b'\n').rev() {
            inspect_reverse(
                line,
                &mut newest,
                &mut found_work,
                &mut found_goal,
                &mut found_preview,
            );
            if found_work && found_goal && found_preview {
                break;
            }
        }
        suffix = bytes[..complete_from.saturating_sub(1)].to_vec();
        cursor = start;
    }
    Ok(newest)
}

fn inspect_reverse(
    line: &[u8],
    newest: &mut Pulse,
    found_work: &mut bool,
    found_goal: &mut bool,
    found_preview: &mut bool,
) {
    if !interesting(line) {
        return;
    }
    let Ok(event) = serde_json::from_slice::<Value>(line) else {
        return;
    };
    if event.get("type").and_then(Value::as_str) != Some("event_msg") {
        return;
    }
    let payload = &event["payload"];
    match payload.get("type").and_then(Value::as_str) {
        Some("task_started" | "turn_started") if !*found_work => {
            newest.running = true;
            *found_work = true;
        }
        Some("task_complete" | "turn_complete" | "turn_aborted") if !*found_work => {
            newest.running = false;
            *found_work = true;
            if !*found_preview
                && let Some(message) = payload.get("last_agent_message").and_then(Value::as_str)
                && !message.trim().is_empty()
            {
                assign_preview(&mut newest.preview, message);
                *found_preview = true;
            }
        }
        Some("thread_goal_updated") if !*found_goal => {
            newest.goal = payload["goal"]["status"].as_str() == Some("active");
            *found_goal = true;
        }
        Some("user_message" | "agent_message") if !*found_preview => {
            if let Some(message) = payload.get("message").and_then(Value::as_str)
                && !message.trim().is_empty()
            {
                assign_preview(&mut newest.preview, message);
                *found_preview = true;
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    fn fixture(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("fixture");
        for line in lines {
            writeln!(file, "{line}").expect("write fixture");
        }
        file
    }

    #[test]
    fn active_goal_dominates_an_active_turn() {
        let file = fixture(&[
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"forge it"}}"#,
            r#"{"type":"event_msg","payload":{"type":"thread_goal_updated","goal":{"status":"active"}}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
        ]);
        let summary = Rollouts::default().read(file.path()).expect("summarize");
        assert_eq!(summary.work, Work::Goal);
        assert_eq!(summary.preview, "forge it");
    }

    #[test]
    fn appended_completion_retires_a_turn_and_replaces_the_preview() {
        let mut file = fixture(&[
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"begin"}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
        ]);
        let mut rollouts = Rollouts::default();
        assert_eq!(
            rollouts.read(file.path()).expect("running").work,
            Work::Turn
        );
        file.write_all(
            b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"last_agent_message\":\"slain\"}}\n",
        )
        .expect("append completion");
        let summary = rollouts.read(file.path()).expect("complete");
        assert_eq!(summary.work, Work::Done);
        assert_eq!(summary.preview, "slain");
    }

    #[test]
    fn an_idle_active_goal_is_not_working() {
        let file = fixture(&[
            r#"{"type":"event_msg","payload":{"type":"thread_goal_updated","goal":{"status":"active"}}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#,
        ]);
        assert_eq!(
            Rollouts::default()
                .read(file.path())
                .expect("summarize")
                .work,
            Work::Done
        );
    }

    #[test]
    fn a_paused_goal_with_an_ordinary_turn_is_green() {
        let file = fixture(&[
            r#"{"type":"event_msg","payload":{"type":"thread_goal_updated","goal":{"status":"paused"}}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
        ]);
        assert_eq!(
            Rollouts::default()
                .read(file.path())
                .expect("summarize")
                .work,
            Work::Turn
        );
    }
}
